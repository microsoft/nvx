//! Cgroup v2 handles through which a guest init or agent places, accounts, kills, and settles the
//! processes of its executions.
//!
//! A [`Layout`] names the two cgroups that a caller uses: the agent's, which holds the caller
//! itself, and the workload cgroup, in which or below which its executions run. The library fixes
//! neither, and refuses a workload cgroup that holds the agent's, so that ending a workload's
//! processes never ends the agent. NVX's managed agent, for example, runs as PID 1 in the
//! hierarchy's root, and runs its workloads in the root's `nvx-exec`.
//!
//! An [`ExecutionCgroup`] is the handle of one execution's cgroup: the workload cgroup itself,
//! for a caller that runs one execution at a time, or a child cgroup of it, for one that runs
//! several, whose workload cgroup then holds no process of its own. Through it, the caller:
//!
//! - [prepares](ExecutionCgroup::prepare) the cgroup, with the resource values that it chooses;
//! - [places](ExecutionCgroup::placement) the execution's process in it, which its descendants
//!   then inherit, however they detach from its process group or session;
//! - checks whether any process [occupies](ExecutionCgroup::is_occupied) it;
//! - [kills](ExecutionCgroup::kill) every process in it with `cgroup.kill`;
//! - and [settles](ExecutionCgroup::settle) it: waits until no process is left.
//!
//! Each of the last three covers a [`Scope`] that the caller chooses: the execution's cgroup
//! alone, or the workload cgroup's whole subtree.
//!
//! Occupancy is the cgroup's `pids.current`, which counts every process that the cgroup charges,
//! a zombie that no process has collected included. The library collects no status: settling
//! calls the caller's own status collector between its checks, as [`process`](super::process)
//! describes.
//!
//! A handle holds the directories of its cgroups open, from when it was
//! [opened](ExecutionCgroup::open), and each operation opens the interface files that it uses in
//! them anew. So a handle stays valid whatever controllers its cgroup gains, and reaches its own
//! cgroups alone, even once the caller removes one and prepares another at its path. A removal,
//! which only a path names, takes its turn with the process's other removals and preparations,
//! so it never reaches such a cgroup either.
//!
//! ```no_run
//! use std::time::Duration;
//!
//! use nvx::guest::cgroup::{ExecutionCgroup, Layout, Scope};
//! use nvx::guest::process::Child;
//! use nvx::guest::spawn::Command;
//!
//! # fn main() -> std::io::Result<()> {
//! let layout = Layout::new("/sys/fs/cgroup/agent", "/sys/fs/cgroup/workloads")?;
//! let cgroup = ExecutionCgroup::prepare(&layout, Some("exec-1"), &[("pids.max", "64")])?;
//! let spawned = Command::new(c"/bin/true".to_owned(), vec![c"true".to_owned()])
//!     .cgroup(cgroup.placement()?)
//!     .spawn()?;
//! let mut child = Child::new(spawned.id())?;
//! child.wait()?;
//! // End whatever the execution left behind, without touching the other executions.
//! cgroup.kill(Scope::Execution)?;
//! cgroup.settle(Scope::Execution, Duration::from_secs(2), || Ok(()))?;
//! cgroup.remove()?;
//! # Ok(())
//! # }
//! ```

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use super::spawn::Cgroup;
use super::sys::{self, Errno, FileDescriptor, Mode, OpenFlags, ProcessId};

/// A cgroup's membership file.
const PROCS: &str = "cgroup.procs";
/// A cgroup's file that kills every process in it and below it.
const KILL: &str = "cgroup.kill";
/// A cgroup's file that enables controllers for its children.
const SUBTREE_CONTROL: &str = "cgroup.subtree_control";
/// The `pids` controller's count of the processes in a cgroup and below it.
const PIDS_CURRENT: &str = "pids.current";
/// The longest canonical `pids.current` that a check reads, with its newline.
const COUNT_CAPACITY: usize = 32;
/// The interval between two checks of a settling cgroup.
const SETTLE_INTERVAL: Duration = Duration::from_millis(10);

/// The turns of the process's removals of execution cgroups, and of its preparations of
/// execution cgroups of their own, which take them one at a time: see
/// [`ExecutionCgroup::remove`].
static TURNS: Mutex<()> = Mutex::new(());

#[cfg(test)]
thread_local! {
    /// What the next turn on this thread runs once it found its cgroup, before it acts on the
    /// cgroup's path: the pause in which a test races it.
    static PAUSE: std::cell::Cell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::Cell::new(None) };
}

/// The cgroups that a guest init or agent uses: its own, and the workload cgroup in which or
/// below which its executions run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    /// The cgroup that holds the agent itself.
    agent: PathBuf,
    /// The cgroup in which or below which the executions run.
    workload: PathBuf,
}

impl Layout {
    /// Names the agent's cgroup, `agent`, and the workload cgroup, `workload`, as the absolute
    /// paths of their directories in a mounted cgroup v2 hierarchy.
    ///
    /// The paths are compared as written, so the caller names both without symbolic links or
    /// other aliases: the check keeps a mistaken layout from ending the agent, not a deliberate
    /// one. A relative path, one with a `..` component, a workload cgroup without a parent, and
    /// one that is the agent's or holds it are `EINVAL`. The workload cgroup is not the
    /// hierarchy's root either, which has none of the interface files that the handles use.
    pub fn new(agent: impl Into<PathBuf>, workload: impl Into<PathBuf>) -> io::Result<Self> {
        let agent = agent.into();
        let workload = workload.into();
        if !well_formed(&agent)
            || !well_formed(&workload)
            || workload.parent().is_none()
            || agent.starts_with(&workload)
        {
            return Err(Errno::EINVAL.into());
        }
        Ok(Self { agent, workload })
    }

    /// Returns the directory of the agent's cgroup.
    pub fn agent(&self) -> &Path {
        &self.agent
    }

    /// Returns the directory of the workload cgroup.
    pub fn workload(&self) -> &Path {
        &self.workload
    }
}

/// The processes that an operation of an [`ExecutionCgroup`] covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    /// The execution's cgroup, and the cgroups below it. For an execution with a cgroup of its
    /// own, the workload's other executions keep running; one that runs in the workload cgroup
    /// itself covers the whole workload subtree.
    Execution,
    /// The workload cgroup, and the whole subtree below it, every execution's cgroup included.
    Workload,
}

/// The handle of one execution's cgroup: the workload cgroup itself, or a child cgroup of it.
///
/// It holds the directory of the execution's cgroup open, and the workload cgroup's for an
/// execution with a cgroup of its own, as [`open`](Self::open) found them, and each operation
/// opens the interface files that it uses in them. So it reaches those cgroups alone, even once
/// another cgroup takes one of their paths: a cgroup that was removed has no interface file any
/// more, and an operation on it fails with `ENOENT`. Clones share the directories, and dropping
/// the last one closes them, but neither kills nor removes the cgroup: the caller does that with
/// [`kill`](Self::kill), [`settle`](Self::settle), which takes its status collector, and
/// [`remove`](Self::remove), and sees their failures.
#[derive(Clone, Debug)]
pub struct ExecutionCgroup {
    /// The path of the execution's cgroup.
    path: PathBuf,
    /// The path of the workload cgroup.
    workload: PathBuf,
    /// Whether the execution has a cgroup of its own below the workload cgroup.
    own: bool,
    /// The directories of the cgroups, which the clones share.
    directories: Arc<Directories>,
}

/// The open directories of an [`ExecutionCgroup`]'s cgroups.
#[derive(Debug)]
struct Directories {
    /// The execution's cgroup.
    execution: FileDescriptor,
    /// The workload cgroup, for an execution with a cgroup of its own; otherwise `execution`
    /// names it.
    workload: Option<FileDescriptor>,
}

impl ExecutionCgroup {
    /// Opens the cgroup of an execution in `layout`, without changing the hierarchy: the
    /// workload cgroup itself, or, with `name`, its child cgroup `name`. It holds the directory
    /// of that cgroup open, and the workload cgroup's for a child cgroup, so both must exist.
    ///
    /// A name that is not one path component, such as an empty one, `.`, `..`, or one with a
    /// `/` or a NUL, is `EINVAL`, and a cgroup that does not exist `ENOENT`.
    pub fn open(layout: &Layout, name: Option<&str>) -> io::Result<Self> {
        let path = execution_path(layout, name)?;
        let workload = name.map(|_| open_directory(&layout.workload)).transpose()?;
        let execution = open_directory(&path)?;
        Ok(Self {
            path,
            workload: layout.workload.clone(),
            own: name.is_some(),
            directories: Arc::new(Directories {
                execution,
                workload,
            }),
        })
    }

    /// Prepares the cgroup of an execution in `layout`, which [`open`](Self::open) names, and
    /// returns its handle.
    ///
    /// The workload cgroup is created unless it exists, and an execution's own cgroup is created
    /// below it, which fails with `EEXIST` if it exists, so that no two executions share one.
    /// Each of them gets the `pids` controller, which counts its processes, and the controller of
    /// each resource: the controllers whose interface files it lacks are enabled in its parent's
    /// `cgroup.subtree_control`, in one write. The workload cgroup's parent must have them, as
    /// the hierarchy's root does. Then each resource's value is written to its file in the
    /// execution's cgroup, in order: `("pids.max", "64")`, for example, limits the execution to
    /// 64 processes. An empty value is written as a newline, as `echo` writes it, so that the
    /// kernel applies or refuses it too. A workload cgroup that gives its executions' cgroups a
    /// domain controller, such as `memory`, holds no process of its own, as cgroup v2 requires.
    ///
    /// A resource file that is not a controller's interface file, `CONTROLLER.NAME`, is `EINVAL`,
    /// before anything changes. A failure once the execution's own cgroup exists, such as a value
    /// that the kernel refuses, removes that cgroup again, so that its name can be prepared anew;
    /// the workload cgroup stays, with the controllers that it enabled, as the executions share
    /// it. The preparation of a cgroup of the execution's own takes its turn with the process's
    /// removals, as [`remove`](Self::remove) describes, so that the handle holds the cgroup that
    /// it created, and a failure removes that cgroup alone.
    pub fn prepare(
        layout: &Layout,
        name: Option<&str>,
        resources: &[(&str, &str)],
    ) -> io::Result<Self> {
        for (file, _) in resources {
            controller(file)?;
        }
        let path = execution_path(layout, name)?;
        let above_workload = layout
            .workload
            .parent()
            .expect("a layout's workload cgroup has a parent");
        let files = || std::iter::once(PIDS_CURRENT).chain(resources.iter().map(|(file, _)| *file));
        create(&layout.workload, false)?;
        provide(&layout.workload, above_workload, files())?;
        // In its turn, no removal of the process removes the new cgroup, so its path names it.
        let _turn = name.map(|_| turn());
        if name.is_some() {
            create(&path, true)?;
            #[cfg(test)]
            pause();
        }
        let prepared = Self::open(layout, name).and_then(|execution| {
            execution.configure(files(), resources)?;
            Ok(execution)
        });
        if prepared.is_err() && name.is_some() {
            // The new cgroup holds no process yet.
            let _ = fs::remove_dir(&path);
        }
        prepared
    }

    /// Gives the execution's own cgroup, if it has one, the interface files `files` through the
    /// workload cgroup, then writes each resource's value to its file, in order.
    fn configure<'a>(
        &self,
        files: impl IntoIterator<Item = &'a str>,
        resources: &[(&str, &str)],
    ) -> io::Result<()> {
        if self.own {
            provide(&self.path, &self.workload, files)?;
        }
        for (file, value) in resources {
            // Writing nothing would reach no controller.
            let value = if value.is_empty() { "\n" } else { value };
            write_in(&self.directories.execution, file, value.as_bytes())?;
        }
        Ok(())
    }

    /// Returns the directory of the execution's cgroup.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the placement that makes a [spawned](super::spawn::Command::cgroup) child join
    /// the execution's cgroup before its program runs, so that the program and its descendants
    /// start in it. The child joins in one of its own launch stages, so the cgroup may not hold
    /// it until its launch reports: see [`Command::cgroup`](super::spawn::Command::cgroup).
    pub fn placement(&self) -> io::Result<Cgroup> {
        Cgroup::open_in(&self.directories.execution)
    }

    /// Moves the process `id`, with all of its threads, into the execution's cgroup. An ID of 0,
    /// or one above `i32::MAX`, is `EINVAL`.
    pub fn place(&self, id: u32) -> io::Result<()> {
        let process = ProcessId::from_positive(i64::from(id))
            .ok_or_else(|| io::Error::from(Errno::EINVAL))?;
        write_in(&self.directories.execution, PROCS, &process.line())
    }

    /// Returns whether `scope` holds a process, a zombie that no process has collected included.
    ///
    /// It reads `pids.current`, which must be one canonical decimal and a newline: another form
    /// is `EPROTO`, and one of more than 31 bytes `EOVERFLOW`.
    ///
    /// A child that the caller spawned with the cgroup's [`placement`](Self::placement) and whose
    /// launch has yet to report may not be in the cgroup yet: the check does not see it then.
    pub fn is_occupied(&self, scope: Scope) -> io::Result<bool> {
        let descriptor = open_file(self.directory(scope), PIDS_CURRENT, OpenFlags::O_RDONLY)?;
        let mut buffer = [0u8; COUNT_CAPACITY];
        let mut length = 0;
        let result = loop {
            match descriptor.read(&mut buffer[length..]) {
                Ok(count) if count > 0 => {
                    length += count;
                    if length < buffer.len() {
                        continue;
                    }
                    break Err(Errno::EOVERFLOW);
                }
                Ok(_) => break parse_count(&buffer[..length]),
                Err(Errno::EINTR) => {}
                Err(error) => break Err(error),
            }
        };
        let occupied = result?;
        descriptor.close()?;
        Ok(occupied)
    }

    /// Kills every process in `scope` with `SIGKILL`, through `cgroup.kill`, which also kills a
    /// process that one of them forks meanwhile. It does not wait for them to end: see
    /// [`settle`](Self::settle).
    ///
    /// A child that the caller spawned with the cgroup's [`placement`](Self::placement) and whose
    /// launch has yet to report may not be in the cgroup yet: the kill does not reach it then.
    pub fn kill(&self, scope: Scope) -> io::Result<()> {
        write_in(self.directory(scope), KILL, b"1")
    }

    /// Waits at most `timeout` until `scope` holds no process, and fails with `ETIMEDOUT` if one
    /// is left then. It ends no process itself: a caller [kills](Self::kill) the scope first, or
    /// waits for its processes to end on their own.
    ///
    /// Before each check, it calls `reap`, the caller's status collector, which collects the
    /// statuses of the caller's children that have ended, and whose failure ends the wait: a
    /// zombie stays in `pids.current` until its parent collects it. A caller whose executions'
    /// orphaned descendants another process collects, such as its PID 1, still collects the
    /// status of each child of its own. A caller that drives its own poll loop checks
    /// [`is_occupied`](Self::is_occupied) there instead.
    pub fn settle(
        &self,
        scope: Scope,
        timeout: Duration,
        mut reap: impl FnMut() -> io::Result<()>,
    ) -> io::Result<()> {
        let deadline = Instant::now().checked_add(timeout);
        loop {
            reap()?;
            if !self.is_occupied(scope)? {
                return Ok(());
            }
            let remaining = match deadline {
                Some(deadline) => deadline.saturating_duration_since(Instant::now()),
                None => SETTLE_INTERVAL,
            };
            if remaining.is_zero() {
                return Err(Errno::ETIMEDOUT.into());
            }
            std::thread::sleep(remaining.min(SETTLE_INTERVAL));
        }
    }

    /// Removes the execution's own cgroup unless it is gone already, and fails with `EBUSY`
    /// while a process, a zombie included, or a cgroup is left in it, so that a removal never
    /// hides a status that the caller has yet to collect. An execution that runs in the workload
    /// cgroup itself leaves it in place.
    ///
    /// The removal names the cgroup by its path, which no other cgroup takes while the cgroup
    /// exists, as cgroup v2 never renames one. So the process's removals take turns, with its
    /// preparations of cgroups of the executions' own: in its turn, a removal finds its cgroup
    /// still there before it removes the cgroup at its path, and a cgroup that the caller
    /// prepared at the path of a removed one stays, even when clones of one handle remove the
    /// cgroup at once.
    ///
    /// The caller places no process in the cgroup while it removes it: neither with
    /// [`place`](Self::place), nor with a child that it spawned with the cgroup's
    /// [`placement`](Self::placement) and whose launch has yet to report. The kernel removes a
    /// cgroup that only zombies occupy, so a process that joined the cgroup after the removal's
    /// check, and ended before the removal, would be a zombie that the removal hides.
    pub fn remove(&self) -> io::Result<()> {
        if !self.own {
            return Ok(());
        }
        let _turn = turn();
        match self.is_occupied(Scope::Execution) {
            Ok(false) => {}
            Ok(true) => return Err(Errno::EBUSY.into()),
            Err(error) if error.kind() == io::ErrorKind::NotFound && self.is_removed() => {
                return Ok(());
            }
            Err(error) => return Err(error),
        }
        #[cfg(test)]
        pause();
        match fs::remove_dir(&self.path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }

    /// Returns whether the execution's cgroup was removed, which leaves its directory without
    /// even the membership file that every cgroup has.
    fn is_removed(&self) -> bool {
        open_file(&self.directories.execution, PROCS, OpenFlags::O_RDONLY)
            .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
    }

    /// Returns the directory of the cgroup that `scope` covers.
    fn directory(&self, scope: Scope) -> &FileDescriptor {
        match (scope, &self.directories.workload) {
            (Scope::Workload, Some(workload)) => workload,
            _ => &self.directories.execution,
        }
    }
}

/// Waits for the process's turn to remove an execution's own cgroup, or to prepare one, which
/// lasts until the guard drops.
fn turn() -> MutexGuard<'static, ()> {
    TURNS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs the pause that a test set for this thread's next turn, if any.
#[cfg(test)]
fn pause() {
    if let Some(pause) = PAUSE.take() {
        pause();
    }
}

/// Returns the path of an execution's cgroup in `layout`: the workload cgroup itself, or its
/// child cgroup `name`, which is one path component, or `EINVAL`.
fn execution_path(layout: &Layout, name: Option<&str>) -> io::Result<PathBuf> {
    let Some(name) = name else {
        return Ok(layout.workload.clone());
    };
    let mut components = Path::new(name).components();
    if name.contains('\0')
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
        || name.contains('/')
    {
        return Err(Errno::EINVAL.into());
    }
    Ok(layout.workload.join(name))
}

/// Opens the directory of the cgroup at `path`, which names that cgroup from then on, whatever
/// takes its path.
fn open_directory(path: &Path) -> io::Result<FileDescriptor> {
    Ok(sys::open(
        &sys::path_c_string(path)?,
        OpenFlags::O_RDONLY | OpenFlags::O_DIRECTORY | OpenFlags::O_CLOEXEC,
        Mode::empty(),
    )?)
}

/// Opens the interface file `file` of the cgroup whose directory `directory` holds, with `flags`.
fn open_file(
    directory: &FileDescriptor,
    file: &str,
    flags: OpenFlags,
) -> io::Result<FileDescriptor> {
    let name = sys::c_string(file.as_bytes())?;
    Ok(sys::open_at(
        directory,
        &name,
        flags | OpenFlags::O_CLOEXEC,
        Mode::empty(),
    )?)
}

/// Writes a whole control value to the interface file `file` of the cgroup whose directory
/// `directory` holds, and reports a failure to close it, with which the kernel may refuse the
/// value.
fn write_in(directory: &FileDescriptor, file: &str, bytes: &[u8]) -> io::Result<()> {
    let descriptor = open_file(directory, file, OpenFlags::O_WRONLY)?;
    descriptor.write_all(bytes)?;
    Ok(descriptor.close()?)
}

/// Returns whether `path` is absolute and has no `..` component, so that its components name
/// the cgroups that it reaches.
fn well_formed(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|component| component == Component::ParentDir)
}

/// Returns the controller of the interface file `file`, `CONTROLLER.NAME`, or `EINVAL` for a
/// file that is not a controller's, such as one of the core `cgroup.*` files.
fn controller(file: &str) -> io::Result<&str> {
    let name = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    };
    match file.split_once('.') {
        Some((controller, rest))
            if controller != "cgroup"
                && name(controller)
                && controller.bytes().all(|byte| !byte.is_ascii_uppercase())
                && rest.split('.').all(name) =>
        {
            Ok(controller)
        }
        _ => Err(Errno::EINVAL.into()),
    }
}

/// Creates the cgroup `path`, which fails with `EEXIST` if it exists and `exclusive` is set.
/// Otherwise an existing cgroup is accepted, and a file in its place is `ENOTDIR`.
pub(crate) fn create(path: &Path, exclusive: bool) -> io::Result<()> {
    match sys::create_directory(&sys::path_c_string(path)?, Mode::from_bits_retain(0o755)) {
        Ok(()) => Ok(()),
        Err(Errno::EEXIST) if !exclusive && fs::metadata(path)?.is_dir() => Ok(()),
        Err(Errno::EEXIST) if !exclusive => Err(Errno::ENOTDIR.into()),
        Err(error) => Err(error.into()),
    }
}

/// Makes sure that the cgroup `cgroup` has each of the interface files `files`: the controllers
/// of those that it lacks are enabled together in `parent`'s `cgroup.subtree_control`, which
/// gives them to every child of `parent`.
pub(crate) fn provide<'a>(
    cgroup: &Path,
    parent: &Path,
    files: impl IntoIterator<Item = &'a str>,
) -> io::Result<()> {
    let mut missing = Vec::new();
    for file in files {
        match fs::metadata(cgroup.join(file)) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let controller = controller(file)?;
                if !missing.contains(&controller) {
                    missing.push(controller);
                }
            }
            Err(error) => return Err(error),
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    let enable = missing
        .iter()
        .map(|controller| format!("+{controller}"))
        .collect::<Vec<_>>()
        .join(" ");
    write(&parent.join(SUBTREE_CONTROL), enable.as_bytes())
}

/// Parses the canonical decimal form of `pids.current`, and returns whether it is nonzero.
fn parse_count(bytes: &[u8]) -> Result<bool, Errno> {
    if bytes.len() < 2
        || bytes.last() != Some(&b'\n')
        || (bytes.first() == Some(&b'0') && bytes.len() != 2)
        || !bytes[..bytes.len() - 1].iter().all(u8::is_ascii_digit)
    {
        return Err(Errno::EPROTO);
    }
    Ok(bytes.first() != Some(&b'0'))
}

/// Writes a whole control value to the existing file `path`, and reports a failure to close it,
/// with which the kernel may refuse the value.
fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let path = sys::path_c_string(path)?;
    let descriptor = sys::open(
        &path,
        OpenFlags::O_WRONLY | OpenFlags::O_CLOEXEC,
        Mode::empty(),
    )?;
    descriptor.write_all(bytes)?;
    Ok(descriptor.close()?)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::ffi::CString;
    use std::io::Read;
    use std::os::fd::AsFd;
    use std::os::unix::process::ExitStatusExt;

    use nix::poll::{PollFd, PollFlags, PollTimeout};

    use super::*;
    use crate::guest::process::Child;
    use crate::guest::spawn::Command;

    /// Returns a command that runs `/bin/sh -c script`.
    fn shell(script: &str) -> Command {
        Command::new(
            c"/bin/sh".to_owned(),
            vec![
                c"sh".to_owned(),
                c"-c".to_owned(),
                CString::new(script).unwrap(),
            ],
        )
    }

    /// Returns the error number of `result`'s error.
    fn error<T: std::fmt::Debug>(result: io::Result<T>) -> Option<i32> {
        result.unwrap_err().raw_os_error()
    }

    /// Returns a fake hierarchy of regular files in a new temporary directory, which the caller
    /// keeps: the root, which holds the agent, with an empty `cgroup.subtree_control`, and the
    /// workload cgroup `workload`, without processes, as the layout of an agent that runs in the
    /// root.
    fn hierarchy() -> (tempfile::TempDir, Layout) {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("cgroup");
        let workload = root.join("workload");
        fs::create_dir_all(&workload).unwrap();
        fs::write(root.join(SUBTREE_CONTROL), b"").unwrap();
        for (file, contents) in [
            (PIDS_CURRENT, b"0\n".as_slice()),
            (KILL, b""),
            (PROCS, b""),
            (SUBTREE_CONTROL, b""),
        ] {
            fs::write(workload.join(file), contents).unwrap();
        }
        let layout = Layout::new(&root, &workload).unwrap();
        (temporary, layout)
    }

    /// Creates the fake cgroup `name` of an execution below `layout`'s workload cgroup, with
    /// the processes that `pids.current` counts, and returns its handle.
    fn fake_execution(layout: &Layout, name: &str, pids: &[u8]) -> ExecutionCgroup {
        let path = layout.workload().join(name);
        fs::create_dir(&path).unwrap();
        for (file, contents) in [(PIDS_CURRENT, pids), (KILL, b""), (PROCS, b"")] {
            fs::write(path.join(file), contents).unwrap();
        }
        ExecutionCgroup::open(layout, Some(name)).unwrap()
    }

    #[test]
    fn layouts_keep_the_agent_out_of_the_workload_subtree() {
        for (agent, workload) in [
            ("/sys/fs/cgroup", "/sys/fs/cgroup/nvx-exec"),
            ("/sys/fs/cgroup/agent", "/sys/fs/cgroup/container"),
            ("/sys/fs/cgroup/agent", "/sys/fs/cgroup/agent-workloads"),
            (
                "/sys/fs/cgroup/workloads/agent",
                "/sys/fs/cgroup/workloads/executions",
            ),
        ] {
            let layout = Layout::new(agent, workload).unwrap();
            assert_eq!(
                (layout.agent(), layout.workload()),
                (Path::new(agent), Path::new(workload))
            );
        }
        for (agent, workload) in [
            ("sys/fs/cgroup", "/sys/fs/cgroup/nvx-exec"),
            ("/sys/fs/cgroup", "sys/fs/cgroup/nvx-exec"),
            ("/sys/fs/cgroup/agent", "/"),
            ("/sys/fs/cgroup/workload", "/sys/fs/cgroup/workload"),
            ("/sys/fs/cgroup/workload/", "/sys/fs/cgroup/workload"),
            ("/sys/fs/cgroup/workload/agent", "/sys/fs/cgroup/workload"),
            ("/sys/fs/cgroup", "/sys/fs/cgroup/nvx-exec/.."),
            (
                "/sys/fs/cgroup/workload/../agent",
                "/sys/fs/cgroup/workload",
            ),
        ] {
            assert_eq!(
                error(Layout::new(agent, workload)),
                Some(Errno::EINVAL as i32),
                "{agent} {workload}"
            );
        }
    }

    #[test]
    fn executions_run_in_the_workload_cgroup_or_in_one_child_of_it() {
        let (_temporary, layout) = hierarchy();
        let workload = ExecutionCgroup::open(&layout, None).unwrap();
        assert_eq!(workload.path(), layout.workload());
        for name in ["exec-1", "exec.1", "..."] {
            fs::create_dir(layout.workload().join(name)).unwrap();
            let execution = ExecutionCgroup::open(&layout, Some(name)).unwrap();
            assert_eq!(execution.path(), layout.workload().join(name));
        }
        for name in ["", ".", "..", "a/b", "/a", "a/", "./a", "a\0b"] {
            assert_eq!(
                error(ExecutionCgroup::open(&layout, Some(name))),
                Some(Errno::EINVAL as i32),
                "{name:?}"
            );
            assert_eq!(
                error(ExecutionCgroup::prepare(&layout, Some(name), &[])),
                Some(Errno::EINVAL as i32),
                "{name:?}"
            );
        }
        // A handle holds its cgroups' directories, which must exist.
        assert_eq!(
            error(ExecutionCgroup::open(&layout, Some("missing"))),
            Some(Errno::ENOENT as i32)
        );
        fs::write(layout.workload().join("file"), b"").unwrap();
        assert_eq!(
            error(ExecutionCgroup::open(&layout, Some("file"))),
            Some(Errno::ENOTDIR as i32)
        );
        let missing = Layout::new(layout.agent(), layout.agent().join("missing")).unwrap();
        assert_eq!(
            error(ExecutionCgroup::open(&missing, None)),
            Some(Errno::ENOENT as i32)
        );
    }

    #[test]
    fn preparation_creates_the_cgroup_and_enables_its_controllers_together() {
        let (temporary, _) = hierarchy();
        let root = temporary.path().join("cgroup");
        let layout = Layout::new(&root, root.join("fresh")).unwrap();
        let control = root.join(SUBTREE_CONTROL);
        // A new cgroup lacks the files of both controllers, which one write enables.
        let failure =
            ExecutionCgroup::prepare(&layout, None, &[("memory.max", "max")]).unwrap_err();
        assert!(layout.workload().is_dir());
        assert_eq!(fs::read(&control).unwrap(), b"+pids +memory");
        // A fake hierarchy gains no file when a controller is enabled.
        assert_eq!(failure.raw_os_error(), Some(Errno::ENOENT as i32));

        // With every file in place, nothing is enabled, and the values are written in order.
        fs::write(&control, b"").unwrap();
        for file in [PIDS_CURRENT, "memory.max", "memory.high"] {
            fs::write(layout.workload().join(file), b"").unwrap();
        }
        let execution = ExecutionCgroup::prepare(
            &layout,
            None,
            &[("memory.max", "1024"), ("memory.high", "512")],
        )
        .unwrap();
        assert_eq!(execution.path(), layout.workload());
        assert_eq!(fs::read(&control).unwrap(), b"");
        assert_eq!(
            fs::read(layout.workload().join("memory.max")).unwrap(),
            b"1024"
        );
        assert_eq!(
            fs::read(layout.workload().join("memory.high")).unwrap(),
            b"512"
        );

        // An existing workload cgroup is accepted, a file in its place is not.
        fs::remove_dir_all(layout.workload()).unwrap();
        fs::write(layout.workload(), b"").unwrap();
        assert_eq!(
            error(ExecutionCgroup::prepare(&layout, None, &[])),
            Some(Errno::ENOTDIR as i32)
        );
    }

    #[test]
    fn invalid_resources_change_nothing() {
        let (temporary, _) = hierarchy();
        let root = temporary.path().join("cgroup");
        let layout = Layout::new(&root, root.join("fresh")).unwrap();
        for file in [
            "cgroup.procs",
            "cgroup.subtree_control",
            "pids",
            ".max",
            "pids.",
            "pids..max",
            "Pids.max",
            "pids/max",
            "../pids.max",
            "pids.max/x",
            "pids.max\n",
        ] {
            assert_eq!(
                error(ExecutionCgroup::prepare(&layout, None, &[(file, "1")])),
                Some(Errno::EINVAL as i32),
                "{file:?}"
            );
        }
        assert!(!layout.workload().exists());
        assert_eq!(fs::read(root.join(SUBTREE_CONTROL)).unwrap(), b"");
        for file in [
            "hugetlb.2MB.max",
            "memory.swap.max",
            "cpuset.cpus.partition",
            "io.max",
        ] {
            assert_eq!(controller(file).unwrap(), file.split('.').next().unwrap());
        }
    }

    #[test]
    fn an_execution_s_own_cgroup_is_new_and_takes_its_controllers_from_the_workload() {
        let (_temporary, layout) = hierarchy();
        let root = layout.agent().to_owned();
        let execution = ExecutionCgroup::prepare(&layout, Some("exec-1"), &[]).unwrap();
        assert!(execution.path().is_dir());
        // The workload cgroup counts its processes already, so only it enables the controller.
        assert_eq!(fs::read(root.join(SUBTREE_CONTROL)).unwrap(), b"");
        assert_eq!(
            fs::read(layout.workload().join(SUBTREE_CONTROL)).unwrap(),
            b"+pids"
        );
        // A second execution of the same name would share the first one's cgroup.
        let duplicate = ExecutionCgroup::prepare(&layout, Some("exec-1"), &[]).unwrap_err();
        assert_eq!(duplicate.kind(), io::ErrorKind::AlreadyExists);
        // A workload cgroup that lacks the controller gets it from its parent first.
        fs::remove_file(layout.workload().join(PIDS_CURRENT)).unwrap();
        ExecutionCgroup::prepare(&layout, Some("exec-2"), &[]).unwrap();
        assert_eq!(fs::read(root.join(SUBTREE_CONTROL)).unwrap(), b"+pids");
    }

    #[test]
    fn a_failed_preparation_leaves_no_cgroup_of_the_execution_s_own() {
        let (_temporary, layout) = hierarchy();
        // No controller creates `pids.max` in a fake cgroup, so its write fails once the
        // execution's cgroup exists.
        let failed = ExecutionCgroup::prepare(&layout, Some("exec-1"), &[("pids.max", "16")]);
        assert_eq!(failed.unwrap_err().kind(), io::ErrorKind::NotFound);
        assert!(!layout.workload().join("exec-1").exists());
        // The name can be prepared anew.
        let execution = ExecutionCgroup::prepare(&layout, Some("exec-1"), &[]).unwrap();
        assert!(execution.path().is_dir());
        // An execution in the workload cgroup itself leaves the shared cgroup in place.
        let failed = ExecutionCgroup::prepare(&layout, None, &[("pids.max", "16")]);
        assert_eq!(failed.unwrap_err().kind(), io::ErrorKind::NotFound);
        assert!(layout.workload().join(PIDS_CURRENT).is_file());
    }

    #[test]
    fn an_empty_resource_value_reaches_its_file_as_a_newline() {
        let (_temporary, layout) = hierarchy();
        let limit = layout.workload().join("pids.max");
        fs::write(&limit, b"").unwrap();
        ExecutionCgroup::prepare(&layout, None, &[("pids.max", "")]).unwrap();
        // The kernel gets a value to apply or refuse, as from `echo`, not a write of nothing.
        assert_eq!(fs::read(&limit).unwrap(), b"\n");
    }

    #[test]
    fn occupancy_is_the_scope_s_canonical_process_count() {
        let (_temporary, layout) = hierarchy();
        let execution = fake_execution(&layout, "exec-1", b"1\n");
        assert!(execution.is_occupied(Scope::Execution).unwrap());
        assert!(!execution.is_occupied(Scope::Workload).unwrap());
        let count = layout.workload().join(PIDS_CURRENT);
        for (contents, expected) in [
            (b"0\n".as_slice(), Ok(false)),
            (b"10\n", Ok(true)),
            (b"4194304\n", Ok(true)),
            (b"", Err(Errno::EPROTO)),
            (b"0", Err(Errno::EPROTO)),
            (b"00\n", Err(Errno::EPROTO)),
            (b"-1\n", Err(Errno::EPROTO)),
            (b" 1\n", Err(Errno::EPROTO)),
            (b"max\n", Err(Errno::EPROTO)),
            (&[b'1'; 40], Err(Errno::EOVERFLOW)),
        ] {
            fs::write(&count, contents).unwrap();
            assert_eq!(
                execution
                    .is_occupied(Scope::Workload)
                    .map_err(|error| Errno::from_raw(error.raw_os_error().unwrap())),
                expected,
                "{contents:?}"
            );
        }
        fs::remove_file(&count).unwrap();
        assert_eq!(
            error(execution.is_occupied(Scope::Workload)),
            Some(Errno::ENOENT as i32)
        );
    }

    #[test]
    fn a_kill_reaches_the_scope_s_cgroup_alone() {
        let (_temporary, layout) = hierarchy();
        let execution = fake_execution(&layout, "exec-1", b"0\n");
        execution.kill(Scope::Execution).unwrap();
        assert_eq!(fs::read(execution.path().join(KILL)).unwrap(), b"1");
        assert_eq!(fs::read(layout.workload().join(KILL)).unwrap(), b"");
        execution.kill(Scope::Workload).unwrap();
        assert_eq!(fs::read(layout.workload().join(KILL)).unwrap(), b"1");
        fs::remove_file(execution.path().join(KILL)).unwrap();
        assert_eq!(
            error(execution.kill(Scope::Execution)),
            Some(Errno::ENOENT as i32)
        );
    }

    #[test]
    fn settlement_collects_statuses_between_its_checks_until_the_deadline() {
        let (_temporary, layout) = hierarchy();
        let execution = fake_execution(&layout, "exec-1", b"2\n");
        let count = execution.path().join(PIDS_CURRENT);
        // Each collection lets one process go, as collecting a zombie does.
        let collections = Cell::new(0);
        execution
            .settle(Scope::Execution, Duration::from_secs(10), || {
                collections.set(collections.get() + 1);
                let left = 3 - collections.get().min(3);
                fs::write(&count, format!("{left}\n"))
            })
            .unwrap();
        assert_eq!(collections.get(), 3);

        fs::write(&count, b"1\n").unwrap();
        let failed = execution.settle(Scope::Execution, Duration::from_secs(10), || {
            Err(Errno::ECHILD.into())
        });
        assert_eq!(error(failed), Some(Errno::ECHILD as i32));
        collections.set(0);
        let timed_out = execution.settle(Scope::Execution, Duration::from_millis(50), || {
            collections.set(collections.get() + 1);
            Ok(())
        });
        assert_eq!(error(timed_out), Some(Errno::ETIMEDOUT as i32));
        assert!(collections.get() > 1);
        fs::write(&count, b"bad\n").unwrap();
        assert_eq!(
            error(execution.settle(Scope::Execution, Duration::ZERO, || Ok(()))),
            Some(Errno::EPROTO as i32)
        );
        // The workload cgroup is empty, whatever its execution's count.
        execution
            .settle(Scope::Workload, Duration::ZERO, || Ok(()))
            .unwrap();
    }

    #[test]
    fn processes_are_placed_in_the_execution_s_cgroup() {
        let (_temporary, layout) = hierarchy();
        let execution = fake_execution(&layout, "exec-1", b"0\n");
        let procs = execution.path().join(PROCS);
        let spawned = shell("exit 0")
            .cgroup(execution.placement().unwrap())
            .spawn()
            .unwrap();
        let mut child = Child::new(spawned.id()).unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(
            fs::read(&procs).unwrap(),
            format!("{}\n", child.id()).into_bytes()
        );
        execution.place(4_194_304).unwrap();
        assert_eq!(fs::read(&procs).unwrap(), b"4194304\n");
        for id in [0, u32::MAX] {
            assert_eq!(error(execution.place(id)), Some(Errno::EINVAL as i32));
        }
    }

    #[test]
    fn a_handle_reaches_its_own_cgroups_alone_once_another_takes_a_path() {
        let (_temporary, layout) = hierarchy();
        let old = fake_execution(&layout, "exec-1", b"0\n");
        let clone = old.clone();
        // The caller removes the cgroup, as `remove` does in a real hierarchy, and prepares
        // another of the same name.
        fs::remove_dir_all(old.path()).unwrap();
        let new = fake_execution(&layout, "exec-1", b"1\n");
        for handle in [&old, &clone] {
            assert_eq!(
                error(handle.kill(Scope::Execution)),
                Some(Errno::ENOENT as i32)
            );
            assert_eq!(
                error(handle.is_occupied(Scope::Execution)),
                Some(Errno::ENOENT as i32)
            );
            assert_eq!(error(handle.place(1)), Some(Errno::ENOENT as i32));
            assert_eq!(error(handle.placement()), Some(Errno::ENOENT as i32));
            handle.remove().unwrap();
        }
        assert!(new.path().is_dir());
        assert_eq!(fs::read(new.path().join(KILL)).unwrap(), b"");
        assert_eq!(fs::read(new.path().join(PROCS)).unwrap(), b"");
        assert!(new.is_occupied(Scope::Execution).unwrap());
        // The workload cgroup is still the one that the old handle opened.
        old.kill(Scope::Workload).unwrap();
        assert_eq!(fs::read(layout.workload().join(KILL)).unwrap(), b"1");
        new.kill(Scope::Execution).unwrap();
        assert_eq!(fs::read(new.path().join(KILL)).unwrap(), b"1");
    }

    #[test]
    fn only_an_empty_cgroup_of_the_execution_s_own_is_removed() {
        let (_temporary, layout) = hierarchy();
        ExecutionCgroup::open(&layout, None)
            .unwrap()
            .remove()
            .unwrap();
        assert!(layout.workload().is_dir());
        let execution = fake_execution(&layout, "exec-1", b"1\n");
        assert_eq!(error(execution.remove()), Some(Errno::EBUSY as i32));
        fs::remove_file(execution.path().join(PIDS_CURRENT)).unwrap();
        assert_eq!(error(execution.remove()), Some(Errno::ENOENT as i32));
        fs::remove_dir_all(execution.path()).unwrap();
        execution.remove().unwrap();
    }

    #[test]
    fn cgroup_operations_leave_an_unrelated_child_s_status() {
        let mut unrelated = std::process::Command::new("/bin/true").spawn().unwrap();
        let process = ProcessId::from_child(&unrelated);
        while !sys::has_exited(process).unwrap() {
            std::thread::yield_now();
        }
        let (_temporary, layout) = hierarchy();
        let workload = ExecutionCgroup::prepare(&layout, None, &[]).unwrap();
        let execution = fake_execution(&layout, "exec-1", b"0\n");
        for cgroup in [&workload, &execution] {
            let spawned = shell("exit 0")
                .cgroup(cgroup.placement().unwrap())
                .spawn()
                .unwrap();
            let mut child = Child::new(spawned.id()).unwrap();
            for scope in [Scope::Execution, Scope::Workload] {
                assert!(!cgroup.is_occupied(scope).unwrap());
                cgroup.kill(scope).unwrap();
                cgroup
                    .settle(scope, Duration::from_secs(10), || {
                        child.try_wait().map(drop)
                    })
                    .unwrap();
            }
            assert!(child.wait().unwrap().success());
        }
        // Nothing collected a status that it did not name.
        assert!(unrelated.try_wait().unwrap().unwrap().success());
    }

    /// A workload cgroup in the real hierarchy, which the test removes, with everything in it,
    /// when it ends.
    struct RealWorkload(Layout);

    impl RealWorkload {
        /// Creates a new workload cgroup below the root of the hierarchy at `/sys/fs/cgroup`,
        /// with the test's own cgroup as the agent's. It returns `None` where the test cannot
        /// change the hierarchy: unless the test runs as root in a cgroup v2 hierarchy with the
        /// `pids` controller, in which it can create a cgroup, as a read-only mount, or one
        /// that a user namespace's root does not own, does not let it.
        fn new(name: &str) -> Option<Self> {
            let root = Path::new("/sys/fs/cgroup");
            let controllers = fs::read_to_string(root.join("cgroup.controllers")).ok()?;
            if !nix::unistd::geteuid().is_root()
                || !controllers
                    .split_whitespace()
                    .any(|controller| controller == "pids")
            {
                return None;
            }
            let own = fs::read_to_string("/proc/self/cgroup").ok()?;
            let own = own.lines().find_map(|line| line.strip_prefix("0::"))?;
            let agent = root.join(own.trim_start_matches('/'));
            let workload = root.join(format!("nvx-test-{}-{name}", std::process::id()));
            // The test's cgroup lies outside the mount's view in another cgroup namespace,
            // where it reads as a path with `..` components, which no layout accepts.
            let layout = Layout::new(agent, workload).ok()?;
            match fs::create_dir(layout.workload()) {
                Ok(()) => Some(Self(layout)),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ReadOnlyFilesystem | io::ErrorKind::PermissionDenied
                    ) =>
                {
                    None
                }
                Err(error) => panic!(
                    "cannot create the test's cgroup {}: {error}",
                    layout.workload().display()
                ),
            }
        }
    }

    impl Drop for RealWorkload {
        /// Kills whatever is left in the workload cgroup, then removes its cgroups.
        fn drop(&mut self) {
            let workload = self.0.workload();
            let _ = write(&workload.join(KILL), b"1");
            for _ in 0..1000 {
                if fs::read(workload.join(PIDS_CURRENT)).is_ok_and(|count| count == b"0\n") {
                    break;
                }
                std::thread::sleep(SETTLE_INTERVAL);
            }
            if let Ok(entries) = fs::read_dir(workload) {
                for entry in entries.flatten() {
                    if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                        let _ = fs::remove_dir(entry.path());
                    }
                }
            }
            let _ = fs::remove_dir(workload);
        }
    }

    /// Returns whether `reader` polls readable, with data or end of file, within `timeout`.
    fn readable(reader: &std::io::PipeReader, timeout: Duration) -> bool {
        let mut descriptors = [PollFd::new(reader.as_fd(), PollFlags::POLLIN)];
        nix::poll::poll(&mut descriptors, PollTimeout::try_from(timeout).unwrap()).unwrap() == 1
    }

    #[test]
    fn descendants_that_hold_pipes_are_found_and_settled() {
        // The test changes the real hierarchy, which needs root; elsewhere it does nothing.
        let Some(workload) = RealWorkload::new("settle") else {
            return;
        };
        let layout = &workload.0;
        let first = ExecutionCgroup::prepare(layout, Some("first"), &[("pids.max", "16")]).unwrap();
        let second = ExecutionCgroup::prepare(layout, Some("second"), &[]).unwrap();
        assert_eq!(
            fs::read_to_string(first.path().join("pids.max")).unwrap(),
            "16\n"
        );
        assert_eq!(
            ExecutionCgroup::prepare(layout, Some("first"), &[])
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        // A value that the kernel refuses, an empty one included, leaves no cgroup behind, so the
        // name can be prepared anew.
        for value in ["-1", ""] {
            assert_eq!(
                error(ExecutionCgroup::prepare(
                    layout,
                    Some("third"),
                    &[("pids.max", value)]
                )),
                Some(Errno::EINVAL as i32),
                "{value:?}"
            );
            assert!(!layout.workload().join("third").exists());
        }
        ExecutionCgroup::prepare(layout, Some("third"), &[("pids.max", "8")])
            .unwrap()
            .remove()
            .unwrap();

        // The first execution's program ends at once, but leaves a descendant in a session of its
        // own, outside the program's process group, which holds the program's output open.
        let (mut output, writer) = std::io::pipe().unwrap();
        let spawned = shell("setsid sleep 60 & echo started")
            .stdout(writer)
            .cgroup(first.placement().unwrap())
            .spawn()
            .unwrap();
        let mut child = Child::with_pidfd(spawned.id()).unwrap();
        // A child joins its cgroup before its program runs, which its launch report waits for.
        assert!(spawned.into_report().wait().unwrap().is_none());
        assert!(child.wait().unwrap().success());
        let mut started = [0u8; 8];
        output.read_exact(&mut started).unwrap();
        assert_eq!(&started, b"started\n");
        assert!(!readable(&output, Duration::from_millis(200)));
        assert!(first.is_occupied(Scope::Execution).unwrap());

        // The second execution keeps running.
        let spawned = shell("exec sleep 60")
            .cgroup(second.placement().unwrap())
            .spawn()
            .unwrap();
        let mut sibling = Child::new(spawned.id()).unwrap();
        assert!(spawned.into_report().wait().unwrap().is_none());

        // The first execution's cgroup finds the descendant: once it is settled, the output ends.
        first.kill(Scope::Execution).unwrap();
        first
            .settle(Scope::Execution, Duration::from_secs(10), || Ok(()))
            .unwrap();
        assert!(readable(&output, Duration::ZERO));
        let mut rest = Vec::new();
        output.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
        first.remove().unwrap();
        assert!(!first.path().exists());
        // A cgroup prepared at the removed one's path is out of the old handle's reach.
        let again = ExecutionCgroup::prepare(layout, Some("first"), &[]).unwrap();
        assert_eq!(
            error(first.kill(Scope::Execution)),
            Some(Errno::ENOENT as i32)
        );
        assert_eq!(
            error(first.is_occupied(Scope::Execution)),
            Some(Errno::ENOENT as i32)
        );
        first.remove().unwrap();
        assert!(again.path().is_dir());
        again.remove().unwrap();
        assert_eq!(sibling.try_wait().unwrap(), None);
        assert!(second.is_occupied(Scope::Execution).unwrap());

        // Settling the whole workload subtree ends the sibling too.
        second.kill(Scope::Workload).unwrap();
        second
            .settle(Scope::Workload, Duration::from_secs(10), || {
                sibling.try_wait().map(drop)
            })
            .unwrap();
        assert_eq!(sibling.status().unwrap().signal(), Some(nix::libc::SIGKILL));
        second.remove().unwrap();
    }

    /// Runs a racer on another thread once this thread's next turn pauses, and lets the turn go
    /// on once the racer has returned, or half a second after its setup, as it does when the turn
    /// holds the racer off. The racer runs `setup` first, which the turn waits for, so that it
    /// takes place within the pause however late the thread starts, then `racer`, with what
    /// `setup` returned. Joining the thread returns what `racer` returned, or `None` if no turn
    /// paused.
    fn race<S, T: Send + 'static>(
        setup: impl FnOnce() -> S + Send + 'static,
        racer: impl FnOnce(S) -> T + Send + 'static,
    ) -> std::thread::JoinHandle<Option<T>> {
        let (go, start) = std::sync::mpsc::channel();
        let (ready, set_up) = std::sync::mpsc::channel();
        let (finished, done) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            start.recv().ok()?;
            let state = setup();
            let _ = ready.send(());
            let result = racer(state);
            let _ = finished.send(());
            Some(result)
        });
        PAUSE.set(Some(Box::new(move || {
            go.send(()).unwrap();
            // A setup that fails drops its sender, which ends the wait at once.
            let _ = set_up.recv_timeout(Duration::from_secs(10));
            let _ = done.recv_timeout(Duration::from_millis(500));
        })));
        thread
    }

    /// Returns what the racer of `thread` returned, once no turn of this thread is left to pause.
    fn finish<T>(thread: std::thread::JoinHandle<Option<T>>) -> T {
        drop(PAUSE.take());
        thread.join().unwrap().expect("the turn paused")
    }

    #[test]
    fn a_removal_leaves_a_cgroup_prepared_at_its_path_by_a_concurrent_one() {
        // The test changes the real hierarchy, which needs root; elsewhere it does nothing.
        let Some(workload) = RealWorkload::new("removals") else {
            return;
        };
        let layout = workload.0.clone();
        let first = ExecutionCgroup::prepare(&layout, Some("exec"), &[]).unwrap();
        let delayed = first.clone();
        // Once the delayed clone found the cgroup empty, the racer removes it through the other
        // clone and prepares another of the same name, unless the delayed removal holds it off.
        let racer = race(
            move || {
                // The delayed removal pauses between its check and its removal.
                assert!(!first.is_occupied(Scope::Execution).unwrap());
                first
            },
            {
                let layout = layout.clone();
                move |first: ExecutionCgroup| {
                    first.remove().unwrap();
                    ExecutionCgroup::prepare(&layout, Some("exec"), &[]).unwrap()
                }
            },
        );
        delayed.remove().unwrap();
        let replacement = finish(racer);
        assert!(replacement.path().is_dir());
        assert!(!replacement.is_occupied(Scope::Execution).unwrap());
        replacement.remove().unwrap();
        assert!(!replacement.path().exists());
    }

    #[test]
    fn a_failed_preparation_removes_its_own_cgroup_alone() {
        // The test changes the real hierarchy, which needs root; elsewhere it does nothing.
        let Some(workload) = RealWorkload::new("preparations") else {
            return;
        };
        let layout = workload.0.clone();
        // A first preparation gives the workload cgroup's children the `pids` controller.
        ExecutionCgroup::prepare(&layout, Some("first"), &[])
            .unwrap()
            .remove()
            .unwrap();
        // Once the preparation created its cgroup, the racer opens that cgroup, within the pause,
        // removes it, and prepares another of the same name, unless the preparation holds it off.
        // The preparation then fails, as the kernel refuses its value, and removes its cgroup
        // again.
        let racer = race(
            {
                let layout = layout.clone();
                move || ExecutionCgroup::open(&layout, Some("exec")).unwrap()
            },
            {
                let layout = layout.clone();
                move |opened: ExecutionCgroup| {
                    opened.remove().unwrap();
                    ExecutionCgroup::prepare(&layout, Some("exec"), &[]).unwrap()
                }
            },
        );
        assert_eq!(
            error(ExecutionCgroup::prepare(
                &layout,
                Some("exec"),
                &[("pids.max", "-1")]
            )),
            Some(Errno::EINVAL as i32)
        );
        let replacement = finish(racer);
        assert!(replacement.path().is_dir());
        assert!(!replacement.is_occupied(Scope::Execution).unwrap());
        replacement.remove().unwrap();
    }
}
