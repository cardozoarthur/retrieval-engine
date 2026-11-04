//! Only processes spawned by this engine are attached; no PID-based kill API.
use crate::document::{error, Error};
use std::{
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    process::Child,
};
use windows_sys::Win32::System::JobObjects::*;

pub(crate) struct NativeJob {
    handle: OwnedHandle,
}
pub(crate) struct JobMemory {
    pub limit: usize,
    pub peak: usize,
}
fn info(
    handle: windows_sys::Win32::Foundation::HANDLE,
) -> Result<JOBOBJECT_EXTENDED_LIMIT_INFORMATION, Error> {
    // SAFETY: POD Windows structure, valid writable length, optional return length null.
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        QueryInformationJobObject(
            handle,
            JobObjectExtendedLimitInformation,
            (&mut info as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of_val(&info) as u32,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(error("extraction job limits unavailable"));
    }
    Ok(info)
}
impl NativeJob {
    pub fn new(memory: usize) -> Result<Self, Error> {
        // SAFETY: unnamed job, default security, ownership transferred once on success.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(error("extraction job creation failed"));
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY
            | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        limits.BasicLimitInformation.ActiveProcessLimit = 1;
        limits.ProcessMemoryLimit = memory;
        // SAFETY: handle is owned, limits points to initialized structure of given size.
        if unsafe {
            SetInformationJobObject(
                handle.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(error("extraction job limit setup failed"));
        }
        Ok(Self { handle })
    }
    pub fn attach(&self, child: &Child) -> Result<(), Error> {
        // SAFETY: both handles remain alive; child was created by our caller and awaits input.
        if unsafe { AssignProcessToJobObject(self.handle.as_raw_handle(), child.as_raw_handle()) }
            == 0
        {
            return Err(error("extraction process could not join its guarded job"));
        }
        Ok(())
    }
    pub fn terminate(&self) {
        // SAFETY: only our unnamed job is targeted. Native calls/threads die with its process.
        unsafe {
            TerminateJobObject(self.handle.as_raw_handle(), 0xE0C0);
        }
    }
    pub fn memory(&self) -> Result<JobMemory, Error> {
        let info = info(self.handle.as_raw_handle())?;
        Ok(JobMemory {
            limit: info.ProcessMemoryLimit,
            peak: info.PeakProcessMemoryUsed,
        })
    }
}
pub(crate) fn verify_current_job(expected: usize) -> Result<(), Error> {
    // NULL queries the immediate job of this process, including nested jobs.
    let info = info(std::ptr::null_mut())?;
    let required = JOB_OBJECT_LIMIT_PROCESS_MEMORY
        | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
    if info.BasicLimitInformation.LimitFlags & required != required
        || info.BasicLimitInformation.ActiveProcessLimit != 1
        || info.ProcessMemoryLimit != expected
    {
        return Err(error(
            "extraction helper is not in the required guarded job",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    struct OwnedChild(Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn spawn(test: &str, cap: usize) -> OwnedChild {
        use std::os::windows::process::CommandExt;
        OwnedChild(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test, "--nocapture"])
                .env("RETRIEVAL_PRIVATE_JOB_PROBE", cap.to_string())
                .creation_flags(0x08000000)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }
    fn wait(child: &mut Child) -> std::process::ExitStatus {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(exit) = child.try_wait().unwrap() {
                return exit;
            }
            assert!(Instant::now() < until, "owned probe did not terminate");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    #[test]
    fn native_commit_limit_denies_allocation_with_positive_control() {
        use windows_sys::Win32::System::Memory::*;
        if let Ok(cap) = std::env::var("RETRIEVAL_PRIVATE_JOB_PROBE") {
            let cap = cap.parse::<usize>().unwrap();
            std::io::stdin().read_exact(&mut [0]).unwrap();
            verify_current_job(cap).unwrap();
            // Real committed virtual memory, no 128MiB physical page writes or fake RSS.
            let allocation = unsafe {
                VirtualAlloc(
                    std::ptr::null(),
                    128 * 1024 * 1024,
                    MEM_COMMIT | MEM_RESERVE,
                    PAGE_READWRITE,
                )
            };
            let allocated = !allocation.is_null();
            if allocated {
                assert_ne!(unsafe { VirtualFree(allocation, 0, MEM_RELEASE) }, 0);
            }
            assert_eq!(allocated, cap >= 256 * 1024 * 1024);
            return;
        }
        for cap in [64 * 1024 * 1024, 256 * 1024 * 1024] {
            let job = NativeJob::new(cap).unwrap();
            let mut child = spawn(
                "native_job::tests::native_commit_limit_denies_allocation_with_positive_control",
                cap,
            );
            job.attach(&child.0).unwrap();
            child.0.stdin.as_mut().unwrap().write_all(b"s").unwrap();
            assert!(
                wait(&mut child.0).success(),
                "native allocation probe failed for cap {cap}"
            );
            let memory = job.memory().unwrap();
            assert_eq!(memory.limit, cap);
            if cap == 256 * 1024 * 1024 {
                assert!(memory.peak >= 128 * 1024 * 1024);
            }
        }
    }
    #[test]
    fn native_kill_on_close_terminates_owned_child() {
        if let Ok(cap) = std::env::var("RETRIEVAL_PRIVATE_JOB_PROBE") {
            std::io::stdin().read_exact(&mut [0]).unwrap();
            verify_current_job(cap.parse().unwrap()).unwrap();
            println!("owned-job-ready");
            std::io::stdout().flush().unwrap();
            std::io::stdin().read_exact(&mut [0]).unwrap();
            panic!("owned child should be killed before another input byte");
        }
        let job = NativeJob::new(64 * 1024 * 1024).unwrap();
        let mut child = spawn(
            "native_job::tests::native_kill_on_close_terminates_owned_child",
            64 * 1024 * 1024,
        );
        job.attach(&child.0).unwrap();
        child.0.stdin.as_mut().unwrap().write_all(b"s").unwrap();
        let stdout = child.0.stdout.take().unwrap();
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            use std::io::BufRead;
            let found = std::io::BufReader::new(stdout)
                .lines()
                .any(|line| line.is_ok_and(|line| line.contains("owned-job-ready")));
            let _ = send.send(found);
        });
        assert!(
            receive.recv_timeout(Duration::from_secs(5)).unwrap(),
            "owned child must reach guarded blocking input"
        );
        assert!(child.0.try_wait().unwrap().is_none());
        drop(job);
        // Windows kill-on-close can report exit code 0. Termination is proved by
        // the blocked child's exit without receiving the second input byte.
        let _ = wait(&mut child.0);
        reader.join().unwrap();
    }
}
