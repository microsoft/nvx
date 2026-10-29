//! Verified references to OpenVMM processes, through which callers wait for them and end them.
//!
//! A [`ProcessRef`] names one process by its ID, its start time, and the [`BootId`] of the host
//! boot during which it started, which together never name another process. [`Process::open`]
//! checks a reference against the host and holds the process that it names through a handle of
//! the operating system: a pidfd on Linux, and a process handle on Windows. Waits and termination
//! go through that handle, so they reach the referenced process, whether or not the caller
//! started it, and never another process that reuses its ID. A process that does not match its
//! reference is never signaled, and neither is one that a reference from an earlier boot names:
//! every process of that boot has exited.
//!
//! [`Process::wait`] sleeps until the exit event of the process: until its pidfd becomes readable
//! or its handle is signaled. A VMM that a caller finds again after the caller restarted, within
//! the same boot, is therefore waited for as a child is. Linux hosts without pidfds, before
//! Linux 5.3 or in sandboxes that refuse them, check the reference at a short interval instead,
//! until the deadline of the wait, and cannot terminate a process that they cannot hold.
//!
//! The [`supervisor`](super::supervisor) launches OpenVMM, and its
//! [`Launched::process`](super::supervisor::Launched::process) holds the child that it started
//! through a handle of its own.

use std::fmt;
use std::io;
#[cfg(windows)]
use std::os::windows::io::BorrowedHandle;
use std::str::FromStr;
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::platform;
use crate::error::Error;

/// Longest wait for a terminated OpenVMM process to exit.
pub(crate) const KILL_TIMEOUT: Duration = Duration::from_secs(10);
/// Interval at which the exit of a process is polled where no exit event serves: the exit of any
/// process on Linux hosts without pidfds, and the exit of a child that a
/// [`Launched`](super::supervisor::Launched) waits for with a timeout on Linux, since
/// [`std::process::Child`] can wait only without one. An exit is noticed at most this late, for
/// about 40 checks a second.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// The identity of one boot of the host, which changes whenever the host restarts.
///
/// On Linux, it is the UUID in `/proc/sys/kernel/random/boot_id`, which the kernel chooses at
/// random at every boot. On Windows, it is the boot counter that Windows keeps in the registry
/// value `BootId` of
/// `HKEY_LOCAL_MACHINE\SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management\PrefetchParameters`,
/// little-endian and zero-extended to 16 bytes. OpenVMM records the same identity in snapshots.
///
/// Its text form, which `Display`, `FromStr`, and serde use, is 32 lowercase hexadecimal digits;
/// parsing also accepts uppercase ones.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct BootId([u8; 16]);

impl BootId {
    /// Returns the identity of the current boot.
    ///
    /// The first identity that it reads serves the rest of the calling process, which cannot
    /// outlive the boot. Fails with [`io::ErrorKind::Unsupported`] on hosts other than Linux and
    /// Windows.
    pub fn current() -> io::Result<Self> {
        /// The identity of the current boot, once read.
        static CURRENT: OnceLock<BootId> = OnceLock::new();
        if let Some(boot) = CURRENT.get() {
            return Ok(*boot);
        }
        let boot = Self(platform::boot_id()?);
        Ok(*CURRENT.get_or_init(|| boot))
    }

    /// Returns the identity whose bytes are `bytes`.
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Returns the bytes of the identity.
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for BootId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0
            .iter()
            .try_for_each(|byte| write!(formatter, "{byte:02x}"))
    }
}

impl fmt::Debug for BootId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "BootId({self})")
    }
}

impl FromStr for BootId {
    type Err = Error;

    /// Parses the text form of a boot identity, failing with
    /// [`ErrorKind::Io`](crate::ErrorKind::Io) for anything other than 32 hexadecimal digits.
    fn from_str(text: &str) -> crate::Result<Self> {
        platform::parse_hex_id(text).map(Self).ok_or_else(|| {
            Error::io(format!(
                "{text:?} is not a boot identity, which is 32 hexadecimal digits"
            ))
        })
    }
}

impl Serialize for BootId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for BootId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// A reference to one process: its ID, its start time, and the boot of the host during which it
/// started, which together never name another process.
///
/// The start time is that of [`platform::process_start_time`]: on Linux, the `starttime` field
/// of `/proc/<pid>/stat`, in clock ticks since boot, and on Windows, the creation time of the
/// process as a 64-bit `FILETIME`. A reference is a claim, which a caller may record and read
/// back after it restarts: [`Process::open`] checks it before anything waits for the process or
/// signals it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProcessRef {
    /// Process ID.
    pub pid: u32,
    /// Start time, in the units of [`platform::process_start_time`].
    pub start_time: u64,
    /// The boot of the host during which the process started.
    pub boot: BootId,
}

/// How a [`Process`] learns that it exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WaitMethod {
    /// The exit event of the operating system: a pidfd on Linux, and the process handle on
    /// Windows. Waits sleep until the process exits or their deadline passes.
    Event,
    /// A check of the process's identity every 25 ms, until the process exits or the deadline of
    /// the wait passes, on Linux hosts without pidfds: before Linux 5.3, or in sandboxes that
    /// refuse them.
    Polling,
}

/// A process that this host found running with the identity of a [`ProcessRef`], held through a
/// handle of the operating system: a pidfd on Linux, and a process handle on Windows.
///
/// The handle keeps waits and termination on this process, whether or not the caller is its
/// parent, even after the process exits and another one reuses its ID. It belongs to the
/// `Process`, which closes it when dropped, and does not keep the process running. Threads may
/// share a `Process` and wait for it at once.
pub struct Process {
    /// The reference that the process matched.
    reference: ProcessRef,
    /// The handle that holds the process.
    handle: platform::ProcessHandle,
}

impl Process {
    /// Finds the process that `reference` names, and returns `None` if it has exited.
    ///
    /// The process has exited when no process has its ID, when the process that has it started
    /// at another time, and when the reference comes from another boot.
    ///
    /// An error means that the state of the process cannot be determined: it may still run.
    /// Fails with [`io::ErrorKind::PermissionDenied`] when the process that has the ID hides its
    /// identity from this user: on Windows, when this user may not query it, and on Linux, when
    /// `/proc` does not show it, as its `hidepid` mount option arranges for the processes of
    /// other users. Fails with [`io::ErrorKind::InvalidInput`] for a process ID that the host
    /// never assigns, such as 0, and with [`io::ErrorKind::Unsupported`] on hosts other than
    /// Linux and Windows.
    pub fn open(reference: &ProcessRef) -> io::Result<Option<Self>> {
        let handle = open_reference(reference, BootId::current, platform::open_process)?;
        Ok(handle.map(|handle| Self {
            reference: reference.clone(),
            handle,
        }))
    }

    /// Returns the process that `handle` holds, whose identity the caller verified to be
    /// `reference`, as a child that it has not reaped has.
    pub(crate) fn held(reference: ProcessRef, handle: platform::ProcessHandle) -> Self {
        Self { reference, handle }
    }

    /// Returns the reference that the process matched.
    pub fn reference(&self) -> &ProcessRef {
        &self.reference
    }

    /// Returns how waits learn that the process exited.
    pub fn wait_method(&self) -> WaitMethod {
        if self.handle.has_exit_event() {
            WaitMethod::Event
        } else {
            WaitMethod::Polling
        }
    }

    /// Waits until the process exits or `deadline` passes, and returns whether it exited.
    ///
    /// A process exits with its last thread. The wait sleeps until the exit event of the
    /// process, or with [`WaitMethod::Polling`], checks its identity at a short interval. An
    /// error means that the wait cannot determine whether the process exited.
    pub fn wait(&self, deadline: Instant) -> io::Result<bool> {
        wait_handle(
            &self.handle,
            self.reference.pid,
            self.reference.start_time,
            deadline,
        )
    }

    /// Returns whether the process has exited, without waiting.
    pub fn has_exited(&self) -> io::Result<bool> {
        self.wait(Instant::now())
    }

    /// Ends the process at once, unless it has exited or is exiting: with `SIGKILL` on Linux, and
    /// on Windows by terminating it with the status 1. It does not wait for the exit; see
    /// [`wait`](Self::wait).
    ///
    /// Fails with [`io::ErrorKind::Unsupported`] with [`WaitMethod::Polling`], whose host cannot
    /// hold the process, rather than signal an ID that another process may have reused, and
    /// with [`io::ErrorKind::PermissionDenied`] when this user may not end the process.
    pub fn kill(&self) -> io::Result<()> {
        self.handle.kill(self.reference.pid)
    }

    /// Closes the process's handle that has the value of this process's handle `ours`, which the
    /// process inherited from this one, and never uses, as a VMM never uses the guards of the
    /// checked files that it inherits on Windows.
    ///
    /// The process loses no other handle: a process that did not inherit `ours` keeps every handle
    /// that it has, and the close fails. A process that has exited has closed its handles, so the
    /// close succeeds. Fails with [`io::ErrorKind::PermissionDenied`] for a process that
    /// [`open`](Self::open) found: only the `Process` of a launch, which
    /// [`Launched::process`](super::supervisor::Launched::process) returns, may close the
    /// process's handles.
    #[cfg(windows)]
    pub(crate) fn close_inherited(&self, ours: BorrowedHandle<'_>) -> io::Result<()> {
        self.handle.close_inherited(self.reference.pid, ours)
    }
}

impl fmt::Debug for Process {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Process")
            .field("reference", &self.reference)
            .field("wait_method", &self.wait_method())
            .finish_non_exhaustive()
    }
}

/// Checks `reference` against the boot that `current_boot` returns, and during the same boot,
/// opens the process with `open`, as [`Process::open`] does.
fn open_reference<H>(
    reference: &ProcessRef,
    current_boot: impl FnOnce() -> io::Result<BootId>,
    open: impl FnOnce(u32, u64) -> io::Result<Option<H>>,
) -> io::Result<Option<H>> {
    if reference.pid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "0 is not the ID of a process",
        ));
    }
    // Every process of an earlier boot has exited.
    if current_boot()? != reference.boot {
        return Ok(None);
    }
    open(reference.pid, reference.start_time)
}

/// Waits until the process that `handle` holds exits or `deadline` passes, and returns whether
/// it exited, checking the identity `pid` and `start_time` where the handle has no exit event.
fn wait_handle(
    handle: &platform::ProcessHandle,
    pid: u32,
    start_time: u64,
    deadline: Instant,
) -> io::Result<bool> {
    match handle.wait(deadline)? {
        Some(exited) => Ok(exited),
        // Each check verifies the identity as `Process::open` does, so that a process that
        // `/proc` hides never counts as exited.
        None => poll_exit(deadline, || {
            Ok(platform::open_process(pid, start_time)?.is_some())
        }),
    }
}

/// Asks `runs` whether the process still runs, at every [`POLL_INTERVAL`] until it does not or
/// `deadline` passes, and returns whether it stopped running: it exited, even if another
/// process reused its ID.
fn poll_exit(deadline: Instant, mut runs: impl FnMut() -> io::Result<bool>) -> io::Result<bool> {
    loop {
        if !runs()? {
            return Ok(true);
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(false);
        }
        thread::sleep(POLL_INTERVAL.min(deadline - now));
    }
}

#[cfg(test)]
mod reference_tests {
    use serde_json::json;

    use super::*;

    /// The boot of the references in these tests.
    const BOOT: BootId = BootId::from_bytes([0x5a; 16]);

    /// Returns a reference to process `pid` started at `start_time` during [`BOOT`].
    fn reference(pid: u32, start_time: u64) -> ProcessRef {
        ProcessRef {
            pid,
            start_time,
            boot: BOOT,
        }
    }

    /// Fails the test if a process is opened.
    fn never_opened(_: u32, _: u64) -> io::Result<Option<()>> {
        panic!("a reference that cannot name a running process is never opened")
    }

    #[test]
    fn boot_identities_have_one_text_form() {
        let text = "00112233445566778899aabbccddeeff";
        let boot: BootId = text.parse().unwrap();
        assert_eq!(
            boot.as_bytes(),
            &[
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff
            ]
        );
        assert_eq!(boot.to_string(), text);
        assert_eq!(format!("{boot:?}"), format!("BootId({text})"));
        assert_eq!(text.to_uppercase().parse::<BootId>().unwrap(), boot);
        for malformed in [
            "",
            "0011",
            &text[1..],
            &format!("{text}0"),
            "00112233-4455-6677-8899-aabbccddeeff",
            "0g112233445566778899aabbccddeeff",
            "+0112233445566778899aabbccddeeff",
            " 0112233445566778899aabbccddeeff",
            "0011223344556677\u{e9}8899aabbccddee",
        ] {
            let error = malformed.parse::<BootId>().unwrap_err();
            assert_eq!(error.kind(), crate::ErrorKind::Io, "{malformed:?}");
        }

        let encoded = serde_json::to_string(&boot).unwrap();
        assert_eq!(encoded, format!("\"{text}\""));
        assert_eq!(serde_json::from_str::<BootId>(&encoded).unwrap(), boot);
        assert!(serde_json::from_str::<BootId>("\"0011\"").is_err());
        assert!(serde_json::from_str::<BootId>("[0, 17]").is_err());
    }

    #[test]
    fn references_record_their_three_parts() {
        let reference = reference(7, 42);
        let encoded = serde_json::to_value(&reference).unwrap();
        assert_eq!(
            encoded,
            json!({"pid": 7, "startTime": 42, "boot": "5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a"})
        );
        assert_eq!(
            serde_json::from_value::<ProcessRef>(encoded).unwrap(),
            reference
        );
        // A reference without its boot, or with a field that this crate does not know, is not one.
        for malformed in [
            json!({"pid": 7, "startTime": 42}),
            json!({"pid": 7, "startTime": 42, "boot": BOOT, "endpoint": "x"}),
            json!({"pid": -7, "startTime": 42, "boot": BOOT}),
        ] {
            assert!(serde_json::from_value::<ProcessRef>(malformed).is_err());
        }
    }

    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn the_current_boot_has_one_identity() {
        let boot = BootId::current().unwrap();
        assert_eq!(BootId::current().unwrap(), boot);
        assert_eq!(BootId::from_bytes(platform::boot_id().unwrap()), boot);
    }

    #[test]
    fn references_from_another_boot_name_exited_processes() {
        let other = BootId::from_bytes([0xa5; 16]);
        let opened = open_reference(&reference(7, 42), || Ok(other), never_opened);
        assert!(opened.unwrap().is_none());
    }

    #[test]
    fn undetermined_identities_are_errors_and_never_opened() {
        // The current boot is unknown.
        let unknown = open_reference(
            &reference(7, 42),
            || Err(io::Error::other("no boot identity")),
            never_opened,
        );
        assert_eq!(unknown.unwrap_err().to_string(), "no boot identity");
        // The state of the process cannot be read.
        let undetermined = open_reference(
            &reference(7, 42),
            || Ok(BOOT),
            |_, _| Err::<Option<()>, _>(io::Error::from(io::ErrorKind::PermissionDenied)),
        );
        assert_eq!(
            undetermined.unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        // No process has the ID 0.
        let zero = open_reference(
            &reference(0, 42),
            || panic!("an invalid reference is not checked against the boot"),
            never_opened,
        );
        assert_eq!(zero.unwrap_err().kind(), io::ErrorKind::InvalidInput);
        // During the same boot, the process is opened with its recorded identity.
        let opened = open_reference(
            &reference(7, 42),
            || Ok(BOOT),
            |pid, start_time| Ok(Some((pid, start_time))),
        );
        assert_eq!(opened.unwrap(), Some((7, 42)));
    }

    #[test]
    fn polling_checks_the_identity_until_the_deadline() {
        let later = Instant::now() + Duration::from_secs(60);
        // The process exits after two checks.
        let mut checks = [Ok(true), Ok(true), Ok(false)].into_iter();
        assert!(poll_exit(later, || checks.next().unwrap()).unwrap());
        // The process runs past the deadline, which bounds the wait.
        let started = Instant::now();
        let deadline = started + Duration::from_millis(80);
        assert!(!poll_exit(deadline, || Ok(true)).unwrap());
        assert!(Instant::now() >= deadline);
        assert!(started.elapsed() < Duration::from_secs(10));
        // A check that cannot determine the state ends the wait with its error, not an exit.
        let error = poll_exit(later, || Err(io::Error::other("unreadable"))).unwrap_err();
        assert_eq!(error.to_string(), "unreadable");
    }

    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn failures_to_end_a_process_that_exited_are_not_failures() {
        let refused = || Err(io::Error::from(io::ErrorKind::PermissionDenied));
        platform::unless_exited(Ok(()), || panic!("a success asks nothing")).unwrap();
        platform::unless_exited(refused(), || Ok(true)).unwrap();
        let running = platform::unless_exited(refused(), || Ok(false));
        assert_eq!(running.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        let unknown = platform::unless_exited(refused(), || Err(io::Error::other("unknown")));
        assert_eq!(unknown.unwrap_err().to_string(), "unknown");
    }

    /// Returns a command that runs for a minute unless it is killed. The tests spawn it under a
    /// `SpawnLock`, so that on Windows it cannot hold the log of a VMM that another test spawns
    /// meanwhile.
    #[cfg(any(target_os = "linux", windows))]
    fn sleeper() -> std::process::Command {
        let mut command = if cfg!(windows) {
            let mut command = std::process::Command::new("ping");
            command.args(["-n", "60", "127.0.0.1"]);
            command
        } else {
            let mut command = std::process::Command::new("sleep");
            command.arg("60");
            command
        };
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null());
        command
    }

    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn polling_follows_a_real_process_until_it_exits() {
        let mut child = platform::inherit::spawn_command(&mut sleeper()).unwrap();
        let pid = child.id();
        let start_time = platform::process_start_time(pid).unwrap().unwrap();
        let runs =
            || -> io::Result<bool> { Ok(platform::open_process(pid, start_time)?.is_some()) };
        let running = poll_exit(Instant::now() + Duration::from_millis(80), runs);
        child.kill().unwrap();
        let exited = poll_exit(Instant::now() + Duration::from_secs(10), runs);
        child.wait().unwrap();
        assert!(!running.unwrap());
        assert!(exited.unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn hosts_without_pidfds_poll_and_never_signal() {
        let mut child = platform::inherit::spawn_command(&mut sleeper()).unwrap();
        let pid = child.id();
        let process = Process {
            reference: ProcessRef {
                pid,
                start_time: platform::process_start_time(pid).unwrap().unwrap(),
                boot: BootId::current().unwrap(),
            },
            handle: platform::ProcessHandle::without_pidfd(),
        };
        assert_eq!(process.wait_method(), WaitMethod::Polling);
        let running = process.wait(Instant::now() + Duration::from_millis(80));
        // Nothing holds the process, so it is not signaled.
        let refused = process.kill();
        let still_running = process.has_exited();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!running.unwrap());
        assert_eq!(refused.unwrap_err().kind(), io::ErrorKind::Unsupported);
        assert!(!still_running.unwrap());
        // Once the child is reaped, its ID names no process, or another one.
        assert!(
            process
                .wait(Instant::now() + Duration::from_secs(10))
                .unwrap()
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::env;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};

    use super::*;
    use crate::host::sys;

    fn sleeper() -> Command {
        let mut command = Command::new("sleep");
        command.arg("60").stdin(Stdio::null());
        command
    }

    #[test]
    fn zombies_run_while_other_threads_remain() {
        // Field 22 of a stat line, the start time, is 22 here; the command name holds a space
        // and a parenthesis.
        let stat = |state: &str| {
            let fields: Vec<String> = (4..=22).map(|field| field.to_string()).collect();
            format!("7 (open vmm)) {state} {}\n", fields.join(" ")).into_bytes()
        };
        let unasked = || -> io::Result<bool> { panic!("only zombies count their threads") };
        let start_time = |state: &str, remain: io::Result<bool>| {
            platform::start_time_from_stat(7, &stat(state), || remain)
        };
        for state in ["R", "S", "D", "T"] {
            assert_eq!(
                platform::start_time_from_stat(7, &stat(state), unasked).unwrap(),
                Some(22)
            );
        }
        assert_eq!(
            platform::start_time_from_stat(7, &stat("X"), unasked).unwrap(),
            None
        );
        assert_eq!(start_time("Z", Ok(true)).unwrap(), Some(22));
        assert_eq!(start_time("Z", Ok(false)).unwrap(), None);
        assert!(start_time("Z", Err(io::Error::other("unreadable"))).is_err());
        assert!(platform::start_time_from_stat(7, b"7 (vmm) S 1 2", unasked).is_err());
        assert!(platform::start_time_from_stat(7, b"garbage", unasked).is_err());
    }

    #[test]
    fn command_names_need_not_be_utf8() {
        // Field 22 of a stat line, the start time, is 22 here.
        let stat = |name: &[u8]| {
            let fields: Vec<String> = (4..=22).map(|field| field.to_string()).collect();
            let mut stat = b"7 (".to_vec();
            stat.extend_from_slice(name);
            stat.extend_from_slice(format!(") S {}\n", fields.join(" ")).as_bytes());
            stat
        };
        let unasked = || -> io::Result<bool> { panic!("only zombies count their threads") };
        // The kernel cuts this name at 15 bytes, inside the two bytes of its last character, and
        // a process may also name itself with bytes that are never UTF-8, here around a
        // parenthesis and a space.
        let cut = &"vmm-reused-pid\u{e9}".as_bytes()[..15];
        for name in [cut, b"\xff) \xfe"] {
            assert!(std::str::from_utf8(name).is_err());
            assert_eq!(
                platform::start_time_from_stat(7, &stat(name), unasked).unwrap(),
                Some(22)
            );
        }
        // The fields after the command name are text.
        let garbled = b"7 (vmm) S \xff 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22\n";
        assert!(platform::start_time_from_stat(7, garbled, unasked).is_err());
    }

    /// Kills and reaps a child process when dropped, even when a test fails.
    struct Reaped(Child);

    impl Drop for Reaped {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn a_process_that_proc_hides_never_counts_as_exited() {
        let mut child = Reaped(sleeper().spawn().unwrap());
        let pid = child.0.id();
        let start_time = platform::process_start_time(pid).unwrap().unwrap();
        let pidfd = || Some(sys::PidFd::open(pid).unwrap());
        // `/proc` hides the process, as `hidepid` hides the processes of other users, while its
        // pidfd, or without one, signal 0, shows that a process has its ID.
        for pidfd in [pidfd(), None] {
            let hidden = platform::check_identity(pid, start_time, pidfd, None);
            assert_eq!(
                hidden.err().unwrap().kind(),
                io::ErrorKind::PermissionDenied
            );
        }
        // Shown, it is held.
        let stat = fs::read(format!("/proc/{pid}/stat")).unwrap();
        let shown = platform::check_identity(pid, start_time, pidfd(), Some(stat));
        assert!(shown.unwrap().is_some());
        // Once it has exited, its pidfd says so, and once it is reaped, no process has its ID.
        let held = pidfd();
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        let exited = platform::check_identity(pid, start_time, held, None);
        assert!(exited.unwrap().is_none());
        let reaped = platform::check_identity(pid, start_time, None, None);
        assert!(reaped.unwrap().is_none());
    }

    #[test]
    fn a_process_runs_until_its_last_thread_exits() {
        // The main thread leaves through pthread_exit, which turns it into a zombie, while
        // another thread sleeps. Python is the only portable way to arrange that here.
        let spawned = Command::new("python3")
            .args([
                "-c",
                "import ctypes, threading, time\n\
                 threading.Thread(target=time.sleep, args=(60,)).start()\n\
                 ctypes.CDLL(None).pthread_exit(None)\n",
            ])
            .stdin(Stdio::null())
            .spawn();
        let mut child = match spawned {
            Ok(child) => Reaped(child),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                eprintln!("skipping: python3 is not installed");
                return;
            }
            Err(error) => panic!("cannot start python3: {error}"),
        };
        let pid = child.0.id();
        let start_time = platform::process_start_time(pid).unwrap().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
            if stat[stat.rfind(')').unwrap() + 1..]
                .trim_start()
                .starts_with('Z')
            {
                break;
            }
            assert!(Instant::now() < deadline, "the main thread did not exit");
            thread::sleep(POLL_INTERVAL);
        }
        assert_eq!(platform::process_start_time(pid).unwrap(), Some(start_time));
        let process = Process::open(&ProcessRef {
            pid,
            start_time,
            boot: BootId::current().unwrap(),
        })
        .unwrap()
        .expect("a process whose main thread exited still runs");
        assert!(
            !process
                .wait(Instant::now() + Duration::from_millis(100))
                .unwrap()
        );
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        assert!(process.wait(Instant::now()).unwrap());
    }

    #[test]
    fn a_process_whose_command_name_is_not_utf8_can_be_examined() {
        // A process takes as its command name the first 15 bytes of the name of the file that it
        // executes: here a link to `sleep` whose 15th byte is the first of a two-byte character.
        // `argv[0]` still says `sleep`, which multi-call executables such as BusyBox read.
        let path = env::var_os("PATH").unwrap_or_default();
        let sleep = env::split_paths(&path)
            .map(|directory| directory.join("sleep"))
            .find(|candidate| candidate.is_file())
            .expect("sleep is on PATH");
        let directory = tempfile::tempdir().unwrap();
        let link = directory.path().join("vmm-reused-pid\u{e9}");
        // A `PATH` entry may be relative to the current directory, but a link resolves a relative
        // target from its own.
        symlink(std::path::absolute(sleep).unwrap(), &link).unwrap();
        let mut child = Reaped(
            Command::new(&link)
                .arg0("sleep")
                .arg("60")
                .stdin(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let pid = child.0.id();
        // The child may take the name only after `spawn` returns.
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::str::from_utf8(&fs::read(format!("/proc/{pid}/stat")).unwrap()).is_ok() {
            assert!(Instant::now() < deadline, "the child did not take the name");
            thread::sleep(POLL_INTERVAL);
        }
        let reference = ProcessRef {
            pid,
            start_time: platform::process_start_time(pid).unwrap().unwrap(),
            boot: BootId::current().unwrap(),
        };
        assert!(Process::open(&reference).unwrap().is_some());
        // A reference to a process that exited and whose ID this one reused names no process.
        let stale = ProcessRef {
            start_time: reference.start_time + 1,
            ..reference
        };
        assert!(Process::open(&stale).unwrap().is_none());
        child.0.kill().unwrap();
        child.0.wait().unwrap();
    }
}
