//! Cgroup v2 handles through which a guest init or agent places, accounts, kills, and settles the
//! processes of its executions.
//!
//! A [`Layout`] names the two cgroups that a caller uses: the agent's, which holds the caller
//! itself, and the workload cgroup, in which or below which its executions run. The library fixes
//! neither, and refuses a workload cgroup that holds the agent's, so that ending a workload's
//! processes never ends the agent. NVX's legacy sandbox, for example, keeps its agents in the
//! `nvx` cgroup and its workloads in the sibling `container` cgroup.
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
            tests::pause();
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
    /// start in it.
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
        tests::pause();
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
mod tests;
