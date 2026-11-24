//! Process-group ownership for detached tmux queries.

#[cfg(unix)]
mod platform {
    use std::io;
    use std::process::Child;

    #[derive(Default)]
    pub(super) struct ProcessGroup {
        leader: Option<libc::pid_t>,
    }

    impl ProcessGroup {
        pub(super) fn attach_std(&mut self, child: &Child) -> io::Result<()> {
            let pid = libc::pid_t::try_from(child.id()).map_err(io::Error::other)?;
            // SAFETY: getpgrp reads the caller's group without arguments or retained pointers.
            let own_group = unsafe { libc::getpgrp() };
            if pid <= 1 || pid == own_group {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Invalid child process group",
                ));
            }
            self.leader = Some(pid);
            Ok(())
        }

        pub(super) fn has_live_members(&self) -> Option<bool> {
            match self.send(0) {
                Ok(()) => Some(true),
                Err(error) => match error.raw_os_error() {
                    Some(libc::ESRCH) => Some(false),
                    Some(libc::EPERM) => Some(true),
                    _ => None,
                },
            }
        }

        pub(super) fn terminate(&self) -> io::Result<()> {
            self.send(libc::SIGTERM)
        }

        pub(super) fn kill(&self) -> io::Result<()> {
            self.send(libc::SIGKILL)
        }

        fn send(&self, signal: libc::c_int) -> io::Result<()> {
            let Some(leader) = self.leader else {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "Missing child process group",
                ));
            };
            // SAFETY: the positive group ID was checked at enrollment, after the child's setsid.
            if unsafe { libc::killpg(leader, signal) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
    use std::process::Child;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> RawHandle;
        fn AssignProcessToJobObject(job: RawHandle, process: RawHandle) -> i32;
        fn TerminateJobObject(job: RawHandle, exit_code: u32) -> i32;
    }

    pub(super) struct ProcessGroup(OwnedHandle);

    impl ProcessGroup {
        pub(super) fn new() -> io::Result<Self> {
            // SAFETY: null parameters request an unnamed job with default permissions.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: a non-null handle is newly owned by this caller.
            Ok(Self(unsafe { OwnedHandle::from_raw_handle(handle) }))
        }

        pub(super) fn attach_std(&mut self, child: &Child) -> io::Result<()> {
            // SAFETY: both handles stay live for the call.
            let result =
                unsafe { AssignProcessToJobObject(self.0.as_raw_handle(), child.as_raw_handle()) };
            check(result)
        }

        pub(super) fn has_live_members(&self) -> Option<bool> {
            None
        }

        pub(super) fn terminate(&self) -> io::Result<()> {
            // SAFETY: the job handle stays live for the call.
            check(unsafe { TerminateJobObject(self.0.as_raw_handle(), 1) })
        }

        pub(super) fn kill(&self) -> io::Result<()> {
            self.terminate()
        }
    }

    impl Drop for ProcessGroup {
        fn drop(&mut self) {
            let _ = self.terminate();
        }
    }

    fn check(result: i32) -> io::Result<()> {
        if result == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

pub(super) struct ProcessGroup(platform::ProcessGroup);

impl ProcessGroup {
    pub(super) fn new() -> std::io::Result<Self> {
        #[cfg(unix)]
        return Ok(Self(platform::ProcessGroup::default()));

        #[cfg(windows)]
        platform::ProcessGroup::new().map(Self)
    }

    pub(super) fn attach_std(&mut self, child: &std::process::Child) -> std::io::Result<()> {
        self.0.attach_std(child)
    }

    pub(super) fn has_live_members(&self) -> Option<bool> {
        self.0.has_live_members()
    }

    pub(super) fn terminate(&self) -> std::io::Result<()> {
        self.0.terminate()
    }

    pub(super) fn kill(&self) -> std::io::Result<()> {
        self.0.kill()
    }
}
