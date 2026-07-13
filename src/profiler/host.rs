// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//! Optional host-trace orchestration (Windows WPR/xperf, Linux perf) tied to VMM lifetime.

use ::std::fs;
use ::std::path::Path;
use ::std::path::PathBuf;
use ::std::process::Command;

use ::log::warn;

#[cfg(target_os = "linux")]
use ::std::time::{Duration, Instant};

use super::ProfilingConfig;
use super::ensure_parent_dir;

/// Optional host-trace session lifecycle.
pub struct HostTraceSession {
    /// Stable, published host-trace path. Only ever created by an atomic rename from `temp_path`
    /// after the recorder finalizes successfully, so its mere existence means a *complete* trace
    /// from *this* run — post-processing can trust it without a freshness check.
    pub output_path: PathBuf,
    /// Unique per-process path the recorder writes into while active. Renamed onto `output_path`
    /// on success, or removed on failure, so a stable trace path is never left stale or partial.
    temp_path: PathBuf,
    active: bool,
    /// Per-invocation provenance id, written into the trace's `.pid` manifest on publish so
    /// `full`-mode post-processing can require the host trace to belong to this run.
    run_id: String,
    /// Fixed WPR instance name (see [`wpr_instance`]) so a session leaked by a hard-killed prior
    /// run is recoverable: `stop` and the next run's start both address it by this constant name.
    #[cfg(target_os = "windows")]
    instance: String,
    /// Materialized bundled `.wprp` file to remove on stop, if one was written.
    #[cfg(target_os = "windows")]
    profile_file: Option<PathBuf>,
    /// Cross-process lock held for the whole capture. The fixed-name WPR recording session is
    /// machine-global, so this serializes host captures across concurrent nvx runs on the whole
    /// machine (a run that cannot acquire it skips host tracing). Dropping the guard releases the
    /// lock; the OS also releases it if the process is hard-killed.
    #[cfg(target_os = "windows")]
    lock: Option<HostCaptureLock>,
    #[cfg(target_os = "linux")]
    child: Option<::std::process::Child>,
}

impl HostTraceSession {
    /// Starts host profiling if enabled in `cfg`.
    pub fn start(cfg: &ProfilingConfig) -> Option<Self> {
        if !cfg.host_profile {
            return None;
        }

        #[cfg(target_os = "windows")]
        {
            let output_path = derive_host_trace_path(&cfg.guest_profile_path, ".host.etl");
            // Serialize host captures across processes before starting the recording session: the
            // fixed-name WPR session is machine-global, so two concurrent nvx host captures would
            // collide on that shared session. If another run holds the lock, skip host tracing
            // (guest-only profiling still proceeds) rather than disturb its capture; the lock
            // releases on `stop`, or via the OS if the process dies.
            let Some(lock) = acquire_host_lock() else {
                warn!(
                    "another nvx host capture is in progress (or the host-profile lock is \
                     inaccessible); skipping host tracing so concurrent runs do not collide on the \
                     machine-global WPR recording session"
                );
                return None;
            };
            // Create the trace directory up front: WPR writes the ETL here on `-stop`, so the
            // path must exist before recording starts (the guest folded parent is created lazily
            // in `write_folded`, which is too late for the host trace).
            if let Err(e) = ensure_parent_dir(&output_path) {
                warn!("failed to create host trace directory: {e}");
                return None;
            }
            // Invalidate any stale trace from a previous run at the stable path: it is only
            // (re)created by the atomic publish in `stop` on success, so if this run fails to
            // finalize, post-processing must not pick up an old file.
            let _ = fs::remove_file(&output_path);
            let _ = fs::remove_file(pid_sidecar_path(&output_path));
            let temp_path = temp_trace_path(&output_path);
            let _ = fs::remove_file(&temp_path);

            // Resolve the recording profile. Precedence: explicit `--wpr-profile` (cfg.wpr_profile)
            // > NVX_WPR_PROFILE env > the bundled default (NvxCpuScheduling). A bundled profile
            // name materializes the embedded `.wprp` next to the trace; any other value (a built-in
            // profile like "CPU", or an explicit "file.wprp!Name") is passed through unchanged.
            let override_val = cfg
                .wpr_profile
                .clone()
                .or_else(|| ::std::env::var("NVX_WPR_PROFILE").ok())
                .filter(|s| !s.trim().is_empty());
            let mut profile_file: Option<PathBuf> = None;
            let profile_arg = match override_val {
                Some(name) if NVX_WPRP_PROFILES.contains(&name.as_str()) => {
                    match write_embedded_wprp(&output_path) {
                        Ok(p) => {
                            let arg = format!("{}!{}", p.display(), name);
                            profile_file = Some(p);
                            arg
                        }
                        Err(e) => {
                            warn!("failed to materialize bundled WPR profile: {e}");
                            return None;
                        }
                    }
                }
                Some(other) => other,
                None => match write_embedded_wprp(&output_path) {
                    Ok(p) => {
                        let arg = format!("{}!{}", p.display(), NVX_WPRP_DEFAULT_PROFILE);
                        profile_file = Some(p);
                        arg
                    }
                    Err(e) => {
                        warn!("failed to materialize bundled WPR profile: {e}");
                        return None;
                    }
                },
            };

            // Record into a dedicated nvx WPR *instance* (a fixed name) rather than the default
            // session, so an unrelated WPR capture is never disturbed. The cross-process `lock`
            // acquired above guarantees no other nvx run holds this instance concurrently, so the
            // fixed name is safe; it also makes a session leaked by a hard-killed prior run
            // recoverable — cancel it first in case such a run left one recording.
            //
            // The WPR CPU sampling interval is deliberately left at whatever the machine already
            // uses: `xperf -SetProfInt` is machine-global and persists past process exit, so a
            // hard-killed run could leak an altered interval to unrelated tools. The `[HOST]` root
            // is self-consistent at that single rate, and guest/host root widths are not comparable
            // across roots anyway (each root is read independently), so aligning the host rate to
            // the guest sampler buys nothing and is not worth mutating machine-global state for.
            let instance = wpr_instance();
            let _ = Command::new("wpr")
                .arg("-cancel")
                .arg("-instancename")
                .arg(&instance)
                .output();
            let out = Command::new("wpr")
                .arg("-start")
                .arg(&profile_arg)
                .arg("-filemode")
                .arg("-instancename")
                .arg(&instance)
                .output();
            match out {
                Ok(o) if o.status.success() => Some(Self {
                    output_path,
                    temp_path,
                    active: true,
                    run_id: cfg.run_id.clone(),
                    instance,
                    profile_file,
                    lock: Some(lock),
                }),
                Ok(o) => {
                    warn!(
                        "wpr -start failed (exit={:?}): {} (a concurrent WPR session may be \
                         active; stop it or run `wpr -cancel -instancename {}`)",
                        o.status.code(),
                        String::from_utf8_lossy(&o.stderr).trim(),
                        instance
                    );
                    if let Some(p) = &profile_file {
                        let _ = fs::remove_file(p);
                    }
                    None
                }
                Err(e) => {
                    warn!("failed to start wpr host trace: {e}");
                    if let Some(p) = &profile_file {
                        let _ = fs::remove_file(p);
                    }
                    None
                }
            }
        }

        #[cfg(target_os = "linux")]
        {
            let output_path = derive_host_trace_path(&cfg.guest_profile_path, ".host.perf.data");
            // Create the trace directory before spawning perf so perf.data is not lost when the
            // guest-profile path points into a not-yet-created nested directory.
            if let Err(e) = ensure_parent_dir(&output_path) {
                warn!("failed to create host trace directory: {e}");
                return None;
            }
            // Invalidate any stale trace at the stable path: it is only (re)created by the atomic
            // publish in `stop` once perf finalizes cleanly, so a failed run must not leave an old
            // perf.data for post-processing to pick up.
            let _ = fs::remove_file(&output_path);
            let _ = fs::remove_file(pid_sidecar_path(&output_path));
            let temp_path = temp_trace_path(&output_path);
            let _ = fs::remove_file(&temp_path);
            let pid = ::std::process::id().to_string();
            let child = match Command::new("perf")
                .arg("record")
                .arg("-F")
                .arg(cfg.sample_hz.to_string())
                // Request the guest sampler's `--profile-hz` but do NOT force it with
                // `--strict-freq`: the `[HOST]` and `[GUEST]` flamegraph roots are read
                // independently (never cross-compared), so the host root stays self-consistent at
                // whatever rate perf actually uses. Letting perf clamp to a kernel-permitted rate
                // (`perf_event_max_sample_rate`) keeps host tracing working on machines with a low
                // cap instead of disabling it over a rate mismatch that no longer matters.
                .arg("-g")
                .arg("-o")
                .arg(&temp_path)
                .arg("-p")
                .arg(pid)
                .spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    warn!("failed to start perf host trace: {e}");
                    let _ = fs::remove_file(&temp_path);
                    return None;
                }
            };
            Some(Self {
                output_path,
                temp_path,
                active: true,
                run_id: cfg.run_id.clone(),
                child: Some(child),
            })
        }

        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        {
            None
        }
    }

    /// Stops the host tracing session, if active.
    pub fn stop(&mut self) {
        if !self.active {
            return;
        }

        #[cfg(target_os = "windows")]
        {
            // WPR writes the ETL into the unique temp path; publish it onto the stable path only
            // after a clean `-stop` that actually produced the file. The temp path deliberately
            // ends in `.etl` (see `temp_trace_path`): WPR requires the `-stop` target to be an
            // `.etl` file, and a non-interactive `-stop` on a differently-named target prompts and
            // is then cancelled by the closed stdin while still returning success — so verify the
            // trace really exists rather than trusting the exit code alone.
            let ok = match Command::new("wpr")
                .arg("-stop")
                .arg(&self.temp_path)
                .arg("-instancename")
                .arg(&self.instance)
                .output()
            {
                Ok(o) if o.status.success() && self.temp_path.exists() => true,
                Ok(o) => {
                    warn!(
                        "wpr -stop did not finalize a trace (exit={:?}, trace_present={}): {}",
                        o.status.code(),
                        self.temp_path.exists(),
                        String::from_utf8_lossy(&o.stderr).trim()
                    );
                    // A failed or prompt-cancelled `-stop` can leave our instance still recording;
                    // cancel it explicitly so no orphaned WPR session is left running.
                    let _ = Command::new("wpr")
                        .arg("-cancel")
                        .arg("-instancename")
                        .arg(&self.instance)
                        .output();
                    false
                }
                Err(e) => {
                    warn!("failed to stop wpr host trace: {e}");
                    let _ = Command::new("wpr")
                        .arg("-cancel")
                        .arg("-instancename")
                        .arg(&self.instance)
                        .output();
                    false
                }
            };
            self.publish(ok);
            // The materialized bundled profile is only needed for `-start`; remove it now so reused
            // output directories are not littered with per-run `.wprp` files.
            if let Some(p) = self.profile_file.take() {
                let _ = fs::remove_file(p);
            }
            // Release the cross-process lock last, only after the WPR session has been fully torn
            // down, so another run never observes a half-torn-down session. Dropping the handle
            // releases the OS lock.
            self.lock = None;
        }

        #[cfg(target_os = "linux")]
        {
            let mut status: Option<::std::process::ExitStatus> = None;
            if let Some(child) = self.child.as_mut() {
                // Ask perf to exit cleanly first so it flushes and finalizes perf.data.
                // SAFETY: child.id() is a live process id while `child` is held.
                unsafe {
                    ::libc::kill(child.id() as i32, ::libc::SIGTERM);
                }
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    match child.try_wait() {
                        Ok(Some(s)) => {
                            status = Some(s);
                            break;
                        }
                        Ok(None) if Instant::now() < deadline => {
                            ::std::thread::sleep(Duration::from_millis(50))
                        }
                        _ => break,
                    }
                }
            }
            if status.is_none() {
                if let Some(child) = self.child.as_mut() {
                    // perf did not exit in time; it was killed, so perf.data may be truncated.
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
            self.child = None;
            // perf finalizes perf.data on SIGTERM and then re-raises SIGTERM to terminate, so it
            // exits *via the signal nvx sent* rather than with status 0. Accept either a clean exit
            // or termination by our own SIGTERM, then require a valid `PERFILE2` file header before
            // publishing: the header is written only once perf has finalized the trace, so a
            // truncated or hard-killed capture (no/short header) is discarded rather than published.
            let terminated_as_expected = status.map(exited_or_sigterm).unwrap_or(false);
            self.publish(terminated_as_expected && perf_data_finalized(&self.temp_path));
        }
        self.active = false;
    }

    /// Publishes (`ok`) or discards the recorded trace.
    ///
    /// On success the unique `temp_path` is atomically renamed onto the stable `output_path`
    /// (clearing any leftover target first) and a `.pid` manifest records this process's id, the
    /// run id, and this executable's image directory, so `full`-mode extraction can require the
    /// host trace to belong to exactly this run (same run id as the guest profile) rather than
    /// trusting a sibling trace by path alone, and can resolve the VMM's own frames from the PDB
    /// beside `microvm.exe`. On failure the partial `temp_path` and any manifest are removed, so
    /// the stable path — already invalidated at start — stays absent and post-processing reports a
    /// missing trace instead of consuming stale data.
    fn publish(&mut self, ok: bool) {
        let pid_sidecar = pid_sidecar_path(&self.output_path);
        if ok {
            let _ = fs::remove_file(&self.output_path);
            if let Err(e) = fs::rename(&self.temp_path, &self.output_path) {
                warn!(
                    "failed to publish host trace ({} -> {}): {e}",
                    self.temp_path.display(),
                    self.output_path.display()
                );
                let _ = fs::remove_file(&self.temp_path);
                let _ = fs::remove_file(&pid_sidecar);
                return;
            }
            // Record this process's own image directory so the post-processor can point
            // `_NT_SYMBOL_PATH` at it: that is where the VMM's PDB sits (next to `microvm.exe`), so
            // xperf resolves the VMM's own frames to function names instead of raw module offsets.
            // Omitted if the executable path is unavailable; the extractor then falls back to the
            // ambient symbol path.
            let image_dir: Option<PathBuf> = ::std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(Path::to_path_buf));
            let manifest = match image_dir {
                Some(dir) => format!(
                    "pid={}\nrun={}\nimage_dir={}\n",
                    ::std::process::id(),
                    self.run_id,
                    dir.display()
                ),
                None => format!("pid={}\nrun={}\n", ::std::process::id(), self.run_id),
            };
            let _ = fs::write(&pid_sidecar, manifest);
        } else {
            let _ = fs::remove_file(&self.temp_path);
            let _ = fs::remove_file(&pid_sidecar);
        }
    }
}

impl Drop for HostTraceSession {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Bundled WPR recording profiles (embedded at build time), materialized next to the trace so the
/// lean/scheduling profile is the default without depending on the working directory.
#[cfg(target_os = "windows")]
const NVX_WPRP: &str = include_str!("../../scripts/nvx-cpu.wprp");

/// Bundled default WPR profile name. Captures CPU sampled-profile stacks plus scheduling events.
#[cfg(target_os = "windows")]
const NVX_WPRP_DEFAULT_PROFILE: &str = "NvxCpuScheduling";

/// Profile names defined in the bundled `.wprp`, selectable by bare name via `--wpr-profile`/env.
#[cfg(target_os = "windows")]
const NVX_WPRP_PROFILES: [&str; 2] = ["NvxCpuScheduling", "NvxCpu"];

/// Fixed WPR instance name for nvx host captures.
///
/// A fixed (rather than per-PID) name makes a session leaked by a hard-killed prior run
/// recoverable: the next run cancels this instance before starting. Concurrency is handled not by
/// name uniqueness but by [`acquire_host_lock`], which serializes host captures so two live runs
/// never operate this instance at once.
#[cfg(target_os = "windows")]
fn wpr_instance() -> String {
    "nvxprofile".to_string()
}

/// RAII guard for the machine-global host-capture mutex. Dropping it releases and closes the mutex;
/// the OS also releases an abandoned mutex if the owning process is hard-killed.
#[cfg(target_os = "windows")]
struct HostCaptureLock {
    handle: ::windows::Win32::Foundation::HANDLE,
}

#[cfg(target_os = "windows")]
impl Drop for HostCaptureLock {
    fn drop(&mut self) {
        use ::windows::Win32::Foundation::CloseHandle;
        use ::windows::Win32::System::Threading::ReleaseMutex;
        // SAFETY: `handle` is a valid mutex handle owned by this guard and acquired exactly once;
        // it is released and closed a single time here on drop.
        unsafe {
            let _ = ReleaseMutex(self.handle);
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Acquires the machine-global host-capture lock, returning a guard that releases it on drop.
///
/// The fixed-name WPR recording session is machine-global, so only one nvx host capture may run at
/// a time across the whole machine. The lock is a named mutex in the `Global\` kernel namespace, so
/// it is shared across every user session (a per-user temp file would not stop a second user from
/// colliding on that global session). A second acquirer gets `WAIT_TIMEOUT` and must skip host
/// tracing. The guard is held for the capture's lifetime; dropping it — or the process exiting,
/// even via hard kill (the mutex is then abandoned and the next run acquires it) — releases the
/// lock. `None` means the lock is held by another run or could not be created.
#[cfg(target_os = "windows")]
fn acquire_host_lock() -> Option<HostCaptureLock> {
    use ::windows::Win32::Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0};
    use ::windows::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
    use ::windows::core::w;
    let name = w!("Global\\nvx-host-profile");
    // SAFETY: `name` is a valid null-terminated wide string literal; the handle is validated below.
    let handle = unsafe { CreateMutexW(None, false, name) }.ok()?;
    // Try to take ownership without blocking (timeout 0); a live capture in another process holds
    // it and yields WAIT_TIMEOUT. WAIT_ABANDONED means a prior owner died without releasing — the
    // mutex is now ours and any half-torn-down global state is cleaned up by the fresh `-start`.
    // SAFETY: `handle` is a valid, open mutex handle.
    let wait = unsafe { WaitForSingleObject(handle, 0) };
    if wait == WAIT_OBJECT_0 || wait == WAIT_ABANDONED {
        Some(HostCaptureLock { handle })
    } else {
        // Held by another run, or a wait failure: do not retain the handle.
        // SAFETY: `handle` is valid and not owned by us; closing it exactly once is correct.
        unsafe {
            let _ = CloseHandle(handle);
        }
        None
    }
}

/// Writes the embedded bundled `.wprp` next to `trace_path`, returning the written file path.
#[cfg(target_os = "windows")]
fn write_embedded_wprp(trace_path: &Path) -> ::std::io::Result<PathBuf> {
    let dir = trace_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let path = dir.join(format!("nvx-wpr-{}.wprp", ::std::process::id()));
    fs::write(&path, NVX_WPRP)?;
    Ok(path)
}

fn derive_host_trace_path(base: &Path, suffix: &str) -> PathBuf {
    let parent = base.parent().unwrap_or_else(|| Path::new(""));
    let stem = base
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("nvx-profile");
    parent.join(format!("{stem}{suffix}"))
}

/// Whether a `perf record` process terminated the way nvx's clean-shutdown asks it to.
///
/// `perf record` installs a SIGTERM handler that finalizes `perf.data` and then re-raises SIGTERM,
/// so a clean shutdown terminates the process *by the signal* — `ExitStatus::success()` is false.
/// Accept either a normal zero exit or termination by the SIGTERM nvx itself sent; any other signal
/// (e.g. SIGKILL from the fallback hard kill) indicates the trace was not finalized.
#[cfg(target_os = "linux")]
fn exited_or_sigterm(status: ::std::process::ExitStatus) -> bool {
    use ::std::os::unix::process::ExitStatusExt;
    status.success() || status.signal() == Some(::libc::SIGTERM)
}

/// Whether `path` begins with the `PERFILE2` magic that a finalized `perf.data` file carries.
///
/// perf writes this header as part of finalization, so its presence confirms perf flushed and
/// closed the trace rather than leaving a truncated one behind. Missing/short files (e.g. from a
/// hard-killed capture) fail the check and are not published.
#[cfg(target_os = "linux")]
fn perf_data_finalized(path: &Path) -> bool {
    use ::std::io::Read;
    let mut magic = [0u8; 8];
    match fs::File::open(path).and_then(|mut f| f.read_exact(&mut magic)) {
        Ok(()) => &magic == b"PERFILE2",
        Err(_) => false,
    }
}

/// Unique, per-process sibling of `output_path` that the recorder writes into while active.
///
/// The trace is renamed onto the stable `output_path` only after it finalizes successfully, so
/// concurrent runs never collide on the temp file and a failed run never leaves a partial trace at
/// the path post-processing consumes. The original extension is preserved (e.g.
/// `foo.host.etl` -> `foo.host.<pid>.partial.etl`): WPR requires the `-stop` target to end in
/// `.etl`, and a non-`.etl` target makes a non-interactive `-stop` prompt and then silently cancel.
fn temp_trace_path(output_path: &Path) -> PathBuf {
    let stem = output_path
        .file_stem()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    let mut name = stem;
    name.push(format!(".{}.partial", ::std::process::id()));
    if let Some(ext) = output_path.extension() {
        name.push(".");
        name.push(ext);
    }
    output_path.with_file_name(name)
}

/// Manifest path holding this run's provenance (`pid=<n>` and `run=<id>` lines), so host-trace
/// extraction can both filter a system-wide trace to exactly this run's process and confirm the
/// trace belongs to the same run as the guest profile rather than a stale trace from a prior run.
fn pid_sidecar_path(output_path: &Path) -> PathBuf {
    let mut name = output_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".pid");
    output_path.with_file_name(name)
}
