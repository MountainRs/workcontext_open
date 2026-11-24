//! Keep the interpreter, launchers and downstream CLIs in one cancellable unit.
use std::{io, process::{Child, ChildStdin, ChildStdout, Command, ExitStatus}};

pub(crate) struct ProcessTree {
    child: Child,
    group: Option<platform::Group>,
}

impl ProcessTree {
    pub(crate) fn spawn(command: &mut Command) -> io::Result<Self> {
        platform::configure(command);
        let mut child = command.spawn()?;
        match platform::attach(&child) {
            Ok(group) => Ok(Self {child, group:Some(group)}),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(error)
            }
        }
    }

    pub(crate) fn take_stdin(&mut self) -> Option<ChildStdin> { self.child.stdin.take() }
    pub(crate) fn take_stdout(&mut self) -> Option<ChildStdout> { self.child.stdout.take() }

    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.child.try_wait()?;
        if status.is_some() { self.group.take(); }
        Ok(status)
    }

    pub(crate) fn stop(&mut self) {
        if self.group.take().is_some() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

impl Drop for ProcessTree { fn drop(&mut self) { self.stop(); } }

#[cfg(unix)]
mod platform {
    use std::{io, os::unix::process::CommandExt, process::{Child, Command}};
    pub(super) struct Group(libc::pid_t);
    pub(super) fn configure(cmd:&mut Command) { cmd.process_group(0); }
    pub(super) fn attach(child:&Child) -> io::Result<Group> { Ok(Group(child.id() as libc::pid_t)) }
    impl Drop for Group {
        fn drop(&mut self) {
            // process_group(0) makes the launched process the leader of a dedicated group.
            unsafe { libc::kill(-self.0, libc::SIGKILL); }
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::{io, mem::size_of, os::windows::{io::{AsRawHandle, FromRawHandle, OwnedHandle}, process::CommandExt}, process::{Child, Command}, ptr};
    use windows_sys::Win32::{Foundation::INVALID_HANDLE_VALUE, System::{
        Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32},
        JobObjects::{AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE},
        Threading::{OpenThread, ResumeThread, CREATE_NO_WINDOW, CREATE_SUSPENDED, THREAD_SUSPEND_RESUME},
    }};

    pub(super) struct Group { _handle: OwnedHandle }

    // Assign before the first instruction; py.exe and venv redirectors can launch Python
    // immediately. Their children inherit membership even without a pipe handshake.
    pub(super) fn configure(cmd:&mut Command) { cmd.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED); }

    pub(super) fn attach(child:&Child) -> io::Result<Group> {
        // SAFETY: the owning handles below close exactly once. The suspended child cannot
        // exit or create a second thread before resume_primary returns successfully.
        unsafe {
            let handle = CreateJobObjectW(ptr::null(), ptr::null());
            if handle.is_null() { return Err(io::Error::last_os_error()); }
            let job = OwnedHandle::from_raw_handle(handle);
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(handle, JobObjectExtendedLimitInformation, (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(), size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32) == 0
                || AssignProcessToJobObject(handle, child.as_raw_handle()) == 0 {
                return Err(io::Error::last_os_error());
            }
            resume_primary(child.id())?;
            Ok(Group { _handle:job })
        }
    }

    fn resume_primary(pid:u32) -> io::Result<()> {
        // std::process exposes the process handle, but closes the primary thread handle.
        unsafe {
            let raw = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD,0);
            if raw == INVALID_HANDLE_VALUE { return Err(io::Error::last_os_error()); }
            let snapshot = OwnedHandle::from_raw_handle(raw);
            let mut entry: THREADENTRY32 = std::mem::zeroed();
            entry.dwSize = size_of::<THREADENTRY32>() as u32;
            let mut present = Thread32First(snapshot.as_raw_handle(), &mut entry);
            while present != 0 {
                if entry.th32OwnerProcessID == pid {
                    let thread = OpenThread(THREAD_SUSPEND_RESUME,0,entry.th32ThreadID);
                    if thread.is_null() { return Err(io::Error::last_os_error()); }
                    let thread = OwnedHandle::from_raw_handle(thread);
                    if ResumeThread(thread.as_raw_handle()) == u32::MAX { return Err(io::Error::last_os_error()); }
                    return Ok(());
                }
                entry.dwSize = size_of::<THREADENTRY32>() as u32;
                present = Thread32Next(snapshot.as_raw_handle(), &mut entry);
            }
            Err(io::Error::new(io::ErrorKind::NotFound,"suspended process thread not found"))
        }
    }
}
