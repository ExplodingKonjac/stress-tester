//! Process execution with time/memory limits and resource measurement.
//!
//! The judged clock is CPU time (user + system), not wall clock, so verdicts do
//! not depend on how busy the machine is. `wall_limit` remains as a backstop for
//! programs that block forever without burning CPU, which no CPU limit can catch.
//!
//! All stdio is redirected to files, so there are no pipes to deadlock on.
//! Unix: `wait4(WNOHANG)` polling gives per-child `rusage`; live CPU time comes
//! from the process' own CPU clock, and a `pre_exec` hook sets the rlimits.
//! Windows: the child runs inside a Job Object carrying a kernel-enforced commit
//! cap; CPU time and peak memory are polled from `GetProcessTimes` and
//! `QueryInformationJobObject`.

use std::ffi::OsString;
use std::path::Path;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::process::{Command, Stdio};

/// How a program ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitKind {
    Code(i32),
    /// Only produced on Unix; Windows has no equivalent.
    #[cfg_attr(windows, allow(dead_code))]
    Signal(i32),
}

/// What to run and under which limits.
pub struct RunSpec<'a> {
    pub argv: &'a [OsString],
    pub stdin_file: Option<&'a Path>,
    pub stdout_file: Option<&'a Path>,
    pub stderr_file: Option<&'a Path>,
    /// CPU time (user + system); `Duration::MAX` disables the check.
    pub cpu_limit: Duration,
    /// Wall-clock backstop, for programs that block without burning CPU.
    pub wall_limit: Duration,
    /// Bytes; `u64::MAX` disables the check.
    pub memory_limit: u64,
}

/// Measured outcome of one run.
pub struct RunStats {
    /// Killed for exceeding a time limit — either kind.
    pub timed_out: bool,
    /// The kill came from `wall_limit` rather than `cpu_limit`. Diagnostic only.
    pub hung: bool,
    pub memory_exceeded: bool,
    /// CPU time (user + system) actually consumed.
    pub cpu_time: Duration,
    pub wall_time: Duration,
    /// Peak memory in bytes (best effort): resident on Unix, committed on Windows,
    /// where the kernel job cap is on commit charge too.
    pub peak_memory: u64,
    pub exit: ExitKind,
}

/// Over the limit, judged at the millisecond granularity the reports print. Any
/// stricter and a kill at 500.4 ms against a 500 ms limit reports itself as
/// "0.500s > 0.500s"; this way the printed comparison is always literally true.
fn over(spent: Duration, limit: Duration) -> bool {
    spent.as_millis() > limit.as_millis()
}

// ---------------------------------------------------------------------------
// Unix backend
// ---------------------------------------------------------------------------

#[cfg(unix)]
const POLL_INTERVAL: Duration = Duration::from_millis(2);

/// Heap ceiling handed to a judged child, as a multiple of its memory limit but
/// never below this floor. Generous on purpose: the poll below is what decides
/// MLE, and this only has to stop a runaway from outrunning it and leaving the
/// OOM killer to pick a victim. The floor is set well above what a language
/// runtime needs before it runs a line — Node wants ~1 GB of private anonymous
/// memory for V8, and a tighter cap kills it during startup.
#[cfg(unix)]
const DATA_LIMIT_FLOOR: u64 = 2 * 1024 * 1024 * 1024;

#[cfg(unix)]
pub fn run(spec: &RunSpec) -> std::io::Result<RunStats> {
    use std::os::unix::process::CommandExt;

    let mut cmd = Command::new(&spec.argv[0]);
    cmd.args(&spec.argv[1..]);
    // Own process group: a terminal Ctrl-C hits stress-tester only, so judged
    // programs are not killed mid-run (we enforce limits ourselves).
    cmd.process_group(0);
    cmd.stdin(open_or_null(spec.stdin_file, false)?);
    cmd.stdout(open_or_null(spec.stdout_file, true)?);
    cmd.stderr(open_or_null(spec.stderr_file, true)?);

    // Kernel-enforced backstops, applied in the child between fork and exec.
    // Skipped for compilation (`u64::MAX`), which keeps the inherited limits.
    if spec.memory_limit != u64::MAX {
        let data = spec
            .memory_limit
            .saturating_mul(4)
            .max(DATA_LIMIT_FLOOR) as libc::rlim_t;
        // SAFETY: the closure runs in the forked child before exec, so it may only
        // call async-signal-safe functions. get/setrlimit are bare syscalls and
        // `last_os_error` only reads errno; nothing here allocates.
        unsafe {
            cmd.pre_exec(move || {
                // The stack goes to *unlimited*, not to the memory limit: deep
                // recursion is ordinary competitive-programming practice and dies
                // at the inherited 8 MB, but glibc derives its default thread
                // stack size from this limit, so any large finite value makes
                // `std::thread` fail with EAGAIN. Unlimited leaves glibc on its own
                // default. Runaway recursion is still caught — stack pages are RSS,
                // so the poll below sees them.
                for (resource, want) in [
                    (libc::RLIMIT_STACK, libc::RLIM_INFINITY),
                    (libc::RLIMIT_DATA, data),
                ] {
                    let mut lim: libc::rlimit = std::mem::zeroed();
                    if libc::getrlimit(resource, &mut lim) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    // Never raise past the inherited hard limit: an unprivileged
                    // process cannot, and failing the spawn over it would be worse
                    // than judging under a tighter cap.
                    lim.rlim_cur = want.min(lim.rlim_max);
                    if libc::setrlimit(resource, &lim) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
    }

    // Note: dropping `child` does not kill or reap it; we reap via wait4.
    let child = cmd.spawn()?;
    let pid = child.id() as libc::pid_t;
    // Resolved once: the clock id is a pure function of the pid.
    let clock = cpu_clock(pid);

    let start = Instant::now();
    let mut timed_out = false;
    let mut hung = false;
    let mut memory_exceeded = false;
    let mut killed = false;
    let mut peak = 0u64;
    let cpu;
    let exit;

    loop {
        let mut status: libc::c_int = 0;
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        let r = unsafe { libc::wait4(pid, &mut status, libc::WNOHANG, &mut ru) };
        if r < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if r == pid {
            peak = peak.max(ru_maxrss_bytes(&ru));
            cpu = ru_cpu_time(&ru);
            exit = decode_status(status);
            break;
        }
        if !killed {
            if let Some(c) = cpu_time(clock)
                && over(c, spec.cpu_limit)
            {
                timed_out = true;
            } else if start.elapsed() >= spec.wall_limit {
                timed_out = true;
                hung = true;
            } else if let Some(vm) = vm_hwm_bytes(pid) {
                peak = peak.max(vm);
                memory_exceeded = vm > spec.memory_limit;
            }
            if timed_out || memory_exceeded {
                killed = true;
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    // Catches a program that crossed the limit and exited inside a single poll
    // interval, and platforms with no live CPU clock to poll.
    if !timed_out && over(cpu, spec.cpu_limit) {
        timed_out = true;
    }

    Ok(RunStats {
        timed_out,
        hung,
        memory_exceeded,
        cpu_time: cpu,
        wall_time: start.elapsed(),
        peak_memory: peak,
        exit,
    })
}

#[cfg(unix)]
fn open_or_null(path: Option<&Path>, write: bool) -> std::io::Result<Stdio> {
    match path {
        Some(p) => {
            if write {
                Ok(Stdio::from(File::create(p)?))
            } else {
                Ok(Stdio::from(File::open(p)?))
            }
        }
        None => Ok(Stdio::null()),
    }
}

#[cfg(unix)]
fn decode_status(status: libc::c_int) -> ExitKind {
    if libc::WIFEXITED(status) {
        ExitKind::Code(libc::WEXITSTATUS(status))
    } else if libc::WIFSIGNALED(status) {
        ExitKind::Signal(libc::WTERMSIG(status))
    } else {
        ExitKind::Code(-1)
    }
}

/// `ru_maxrss` is KB on Linux and bytes on macOS.
#[cfg(all(unix, target_os = "linux"))]
fn ru_maxrss_bytes(ru: &libc::rusage) -> u64 {
    ru.ru_maxrss.max(0) as u64 * 1024
}

#[cfg(all(unix, not(target_os = "linux")))]
fn ru_maxrss_bytes(ru: &libc::rusage) -> u64 {
    ru.ru_maxrss.max(0) as u64
}

/// Total CPU time (user + system) of a reaped child.
#[cfg(unix)]
fn ru_cpu_time(ru: &libc::rusage) -> Duration {
    let tv = |t: libc::timeval| {
        Duration::new(
            t.tv_sec.max(0) as u64,
            (t.tv_usec.clamp(0, 999_999) as u32) * 1000,
        )
    };
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

/// The child's own CPU clock, for a live time-limit poll (Linux only).
#[cfg(all(unix, target_os = "linux"))]
fn cpu_clock(pid: libc::pid_t) -> Option<libc::clockid_t> {
    let mut clk: libc::clockid_t = 0;
    // Returns an errno directly, not -1.
    (unsafe { libc::clock_getcpuclockid(pid, &mut clk) } == 0).then_some(clk)
}

#[cfg(all(unix, target_os = "linux"))]
fn cpu_time(clock: Option<libc::clockid_t>) -> Option<Duration> {
    let clk = clock?;
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    (unsafe { libc::clock_gettime(clk, &mut ts) } == 0).then(|| {
        Duration::new(
            ts.tv_sec.max(0) as u64,
            ts.tv_nsec.clamp(0, 999_999_999) as u32,
        )
    })
}

/// With no live clock to poll, the CPU limit is enforced post-hoc from `rusage`.
#[cfg(all(unix, not(target_os = "linux")))]
fn cpu_clock(_pid: libc::pid_t) -> Option<libc::clockid_t> {
    None
}

#[cfg(all(unix, not(target_os = "linux")))]
fn cpu_time(_clock: Option<libc::clockid_t>) -> Option<Duration> {
    None
}

/// Live peak-RSS poll for an early memory-limit kill (Linux only).
#[cfg(all(unix, target_os = "linux"))]
fn vm_hwm_bytes(pid: libc::pid_t) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|l| {
            l.strip_prefix("VmHWM:")
                .map(|v| v.trim_end_matches("kB").trim())
        })
        .and_then(|v| v.parse::<u64>().ok())
        .map(|kb| kb * 1024)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn vm_hwm_bytes(_pid: libc::pid_t) -> Option<u64> {
    None
}

// ---------------------------------------------------------------------------
// Windows backend (Job Object)
// ---------------------------------------------------------------------------

#[cfg(windows)]
const POLL_INTERVAL_MS: u32 = 2;

/// The kernel job cap sits at the judged limit plus the larger of the limit itself
/// and this floor. That gap is not slack for the poll below: the kernel checks
/// every commit call, so a *single* allocation landing inside the gap succeeds and
/// the poll reports MLE, while anything past the cap is refused outright and
/// surfaces as `std::bad_alloc` — an RE. A generous gap keeps the common
/// wrong-length `vector<int> v(n)` an MLE instead of a crash.
#[cfg(windows)]
const MEMORY_HEADROOM_MIN: u64 = 64 * 1024 * 1024;

/// Closes a Windows handle on drop, so an early return cannot leak it. Used for
/// the job handle, which `run` owns from creation until it returns.
#[cfg(windows)]
struct OwnedHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

/// Peak committed memory of the job. `PeakJobMemoryUsed` covers the processes
/// still associated with the job and `PeakProcessMemoryUsed` any that ever were,
/// so the larger of the two is right both during the run and after the child has
/// exited.
#[cfg(windows)]
fn job_peak_memory(job: windows_sys::Win32::Foundation::HANDLE) -> Option<u64> {
    use windows_sys::Win32::System::JobObjects::{
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        QueryInformationJobObject,
    };

    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        QueryInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &mut info as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then(|| info.PeakJobMemoryUsed.max(info.PeakProcessMemoryUsed) as u64)
}

/// CPU time (kernel + user) of a process. The handle stays valid after the
/// process exits, so this can also be read once it is gone.
#[cfg(windows)]
fn process_cpu_time(process: windows_sys::Win32::Foundation::HANDLE) -> Option<Duration> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetProcessTimes;

    let mut created: FILETIME = unsafe { std::mem::zeroed() };
    let mut exited: FILETIME = unsafe { std::mem::zeroed() };
    let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
    let mut user: FILETIME = unsafe { std::mem::zeroed() };
    let ok =
        unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) };
    // FILETIME durations are in 100 ns units.
    let ticks = |t: FILETIME| ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64;
    (ok != 0).then(|| Duration::from_nanos((ticks(kernel) + ticks(user)) * 100))
}

#[cfg(windows)]
pub fn run(spec: &RunSpec) -> std::io::Result<RunStats> {
    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_JOB_MEMORY,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_SUSPENDED, CreateProcessW, GetExitCodeProcess, PROCESS_INFORMATION, ResumeThread,
        STARTF_USESTDHANDLES, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
    };

    const WAIT_TIMEOUT: u32 = 0x0000_0102;
    const CREATE_ALWAYS: u32 = 2;

    fn to_wide(s: &std::ffi::OsStr) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    let sa = unsafe {
        let mut sa = std::mem::zeroed::<SECURITY_ATTRIBUTES>();
        sa.nLength = std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32;
        sa.bInheritHandle = 1; // BOOL
        sa
    };

    let open_file = |path: Option<&Path>, write: bool| -> std::io::Result<*mut core::ffi::c_void> {
        let (name, access, disposition) = match (path, write) {
            (Some(p), false) => (to_wide(p.as_os_str()), GENERIC_READ, OPEN_EXISTING),
            (Some(p), true) => (to_wide(p.as_os_str()), GENERIC_WRITE, CREATE_ALWAYS),
            (None, _) => (to_wide("NUL".as_ref()), GENERIC_READ, OPEN_EXISTING),
        };
        let h = unsafe {
            CreateFileW(
                name.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                &sa,
                disposition,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        Ok(h)
    };

    // The job carries the kernel-enforced memory cap. Set it up before any file
    // handle exists, and hold it in a guard, so no early return leaks a handle.
    let raw_job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if raw_job.is_null() || raw_job == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    let job = OwnedHandle(raw_job);
    // Judged programs only. Compilation passes `u64::MAX` and keeps a bare job:
    // MSVC's `mspdbsrv.exe` is meant to outlive a build and be shared between
    // concurrent ones, and KILL_ON_JOB_CLOSE would take it down with us.
    if spec.memory_limit != u64::MAX {
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        // One call, deliberately: this replaces LimitFlags wholesale, so a second
        // SetInformationJobObject would silently drop whatever it does not repeat.
        // KILL_ON_JOB_CLOSE means nothing in the job outlives it, grandchildren
        // included — which is also why every figure must be read before it closes.
        info.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_JOB_MEMORY | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // Saturating: SIZE_T is 32 bits on 32-bit Windows.
        info.JobMemoryLimit = spec
            .memory_limit
            .saturating_add(spec.memory_limit.max(MEMORY_HEADROOM_MIN))
            .min(usize::MAX as u64) as usize;
        let ok = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }

    let h_stdin = open_file(spec.stdin_file, false)?;
    let h_stdout = open_file(spec.stdout_file, true)?;
    let h_stderr = open_file(spec.stderr_file, true)?;

    // Build the quoted command line.
    let cmdline = spec
        .argv
        .iter()
        .map(|a| format!("\"{}\"", a.to_string_lossy().replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(" ");
    let mut cmdline_wide = to_wide(cmdline.as_ref());

    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    si.dwFlags = STARTF_USESTDHANDLES;
    si.hStdInput = h_stdin;
    si.hStdOutput = h_stdout;
    si.hStdError = h_stderr;
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

    let created = unsafe {
        CreateProcessW(
            std::ptr::null(),
            cmdline_wide.as_mut_ptr(),
            &sa,
            &sa,
            1, // inherit handles
            CREATE_SUSPENDED,
            std::ptr::null(),
            std::ptr::null(),
            &si,
            &mut pi,
        )
    };

    let result = (|| -> std::io::Result<RunStats> {
        if created == 0 {
            return Err(std::io::Error::last_os_error());
        }
        unsafe {
            // Parent-side stdio handles are no longer needed.
            CloseHandle(h_stdin);
            CloseHandle(h_stdout);
            CloseHandle(h_stderr);
        }
        // The cap only exists if the process is actually in the job, so a failure
        // here cannot be ignored. The child is still suspended and outside the
        // job, so `KILL_ON_JOB_CLOSE` would not reach it: kill it directly.
        if unsafe { AssignProcessToJobObject(job.0, pi.hProcess) } == 0 {
            let e = std::io::Error::last_os_error();
            unsafe { TerminateProcess(pi.hProcess, 1) };
            return Err(e);
        }
        // Failure here would leave the child suspended forever, and the only thing
        // that would notice is the wall backstop, 10 s later, once per test.
        if unsafe { ResumeThread(pi.hThread) } == u32::MAX {
            let e = std::io::Error::last_os_error();
            unsafe { TerminateJobObject(job.0, 1) };
            return Err(e);
        }

        let start = Instant::now();
        let mut timed_out = false;
        let mut hung = false;
        let mut memory_exceeded = false;
        let mut peak = 0u64;
        loop {
            let wait = unsafe { WaitForSingleObject(pi.hProcess, POLL_INTERVAL_MS) };
            if wait != WAIT_TIMEOUT {
                // Exited, or the wait itself failed — either way, stop polling.
                break;
            }
            if process_cpu_time(pi.hProcess).is_some_and(|c| over(c, spec.cpu_limit)) {
                timed_out = true;
            } else if start.elapsed() >= spec.wall_limit {
                timed_out = true;
                hung = true;
            } else if let Some(m) = job_peak_memory(job.0) {
                peak = peak.max(m);
                memory_exceeded = peak > spec.memory_limit;
            }
            if timed_out || memory_exceeded {
                unsafe {
                    TerminateJobObject(job.0, 1);
                    WaitForSingleObject(pi.hProcess, u32::MAX);
                }
                break;
            }
        }

        // The handle stays valid after exit, so this is the exact final figure.
        let cpu = process_cpu_time(pi.hProcess).unwrap_or_default();
        if !timed_out && over(cpu, spec.cpu_limit) {
            timed_out = true;
        }
        peak = peak.max(job_peak_memory(job.0).unwrap_or(0));

        let mut exit_code: u32 = 0;
        unsafe { GetExitCodeProcess(pi.hProcess, &mut exit_code) };

        Ok(RunStats {
            timed_out,
            hung,
            // Still post-hoc as well: a burst inside one poll interval can cross
            // the limit without the loop ever seeing it.
            memory_exceeded: memory_exceeded || peak > spec.memory_limit,
            cpu_time: cpu,
            wall_time: start.elapsed(),
            peak_memory: peak,
            exit: ExitKind::Code(exit_code as i32),
        })
    })();

    unsafe {
        if created != 0 {
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
        }
    }
    // `job` is dropped after this, closing the job last: with
    // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE that also reaps anything left inside it.
    if created == 0 {
        unsafe {
            CloseHandle(h_stdin);
            CloseHandle(h_stdout);
            CloseHandle(h_stderr);
        }
    }
    result
}

/// Enforcement tests for the live CPU clock, which only Linux polls; elsewhere
/// the CPU limit is applied post-hoc and these timings would not hold.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn run_limited(argv: &[&str], cpu_ms: u64, wall_ms: u64) -> RunStats {
        let argv: Vec<OsString> = argv.iter().map(|a| OsString::from(*a)).collect();
        run(&RunSpec {
            argv: &argv,
            stdin_file: None,
            stdout_file: None,
            stderr_file: None,
            cpu_limit: Duration::from_millis(cpu_ms),
            wall_limit: Duration::from_millis(wall_ms),
            memory_limit: u64::MAX,
        })
        .expect("failed to spawn")
    }

    #[test]
    fn cpu_limit_kills_a_busy_loop() {
        let s = run_limited(&["sh", "-c", "while :; do :; done"], 200, 5_000);
        assert!(s.timed_out, "a busy loop must hit the CPU limit");
        assert!(!s.hung, "the wall backstop must not be what killed it");
        assert!(
            s.cpu_time >= Duration::from_millis(150),
            "cpu {:?}",
            s.cpu_time
        );
        assert!(
            s.wall_time < Duration::from_secs(2),
            "killed late: wall {:?}",
            s.wall_time
        );
    }

    #[test]
    fn wall_backstop_kills_an_idle_process() {
        let s = run_limited(&["sleep", "5"], 200, 300);
        assert!(s.timed_out && s.hung, "sleep must trip the wall backstop");
        assert!(
            s.cpu_time < Duration::from_millis(100),
            "sleeping burns no cpu, got {:?}",
            s.cpu_time
        );
    }
}
