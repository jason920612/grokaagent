//! Process-group / Job Object so killing a worker also kills its grandchildren.

use std::io;

#[cfg(windows)]
mod windows_job {
    use std::io;
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject, TerminateJobObject,
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    pub struct Job(HANDLE);

    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        pub fn new() -> io::Result<Self> {
            let handle = unsafe { CreateJobObjectW(null_mut(), null_mut()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if ok == 0 {
                unsafe { CloseHandle(handle) };
                return Err(io::Error::last_os_error());
            }
            Ok(Self(handle))
        }

        /// Kill every process in the job now.
        pub fn terminate(&self) {
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }

        /// Stop killing the job's processes when the handle closes.
        pub fn release(&self) {
            let info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            unsafe {
                SetInformationJobObject(
                    self.0,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
            }
        }

        pub fn assign_handle(&self, process: HANDLE) -> io::Result<()> {
            let ok = unsafe { AssignProcessToJobObject(self.0, process) };
            if ok == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

pub struct ProcessGuard {
    child: tokio::process::Child,
    #[cfg(windows)]
    _job: windows_job::Job,
    /// Released: dropping no longer kills the process tree.
    detached: bool,
    /// The child holds a lifeline (its stdin): on Unix, dropping closes it
    /// and leaves the child to shut down cleanly instead of killing it.
    lifeline: bool,
}

impl ProcessGuard {
    pub fn spawn(mut cmd: tokio::process::Command) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            cmd.process_group(0);
        }
        #[cfg(windows)]
        let job = windows_job::Job::new()?;
        let child = cmd.spawn()?;
        #[cfg(windows)]
        {
            let handle = child
                .raw_handle()
                .ok_or_else(|| io::Error::other("child has no process handle"))?;
            job.assign_handle(handle as _)?;
            Ok(Self { child, _job: job, detached: false, lifeline: false })
        }
        #[cfg(not(windows))]
        {
            Ok(Self { child, detached: false, lifeline: false })
        }
    }

    /// The command finished on its own: leave anything it started (a GUI
    /// app, a `&` background job) running instead of killing the tree.
    pub fn detach(mut self) {
        self.detached = true;
        #[cfg(windows)]
        self._job.release();
    }

    /// The child was started with a lifeline (`--lifeline`, stdin piped
    /// and kept open, `kill_on_drop(false)` on Unix): when this guard is
    /// dropped without [`Self::finish`] (the parent is quitting), the child
    /// sees its stdin close and ends its own session, so the processes its
    /// tools started (each in a process group of its own) stop too, which
    /// a SIGKILL of its group would leave running. On Windows the job
    /// object reaps the whole tree, so dropping kills as before.
    pub fn with_lifeline(mut self) -> Self {
        self.lifeline = true;
        self
    }

    /// End a child with a lifeline: close it, give the child `grace` to
    /// shut down cleanly, then kill whatever is left.
    pub async fn finish(&mut self, grace: std::time::Duration) {
        drop(self.child.stdin.take());
        let _ = tokio::time::timeout(grace, self.child.wait()).await;
        let _ = self.kill().await;
    }

    pub fn child_mut(&mut self) -> &mut tokio::process::Child {
        &mut self.child
    }

    pub async fn kill(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        {
            if let Some(pid) = self.child.id() {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
        }
        #[cfg(windows)]
        self._job.terminate();
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
        Ok(())
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if self.detached {
            return;
        }
        if self.lifeline && cfg!(unix) {
            drop(self.child.stdin.take());
            return;
        }
        let _ = self.child.start_kill();
        #[cfg(unix)]
        {
            if let Some(pid) = self.child.id() {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
        }
    }
}
