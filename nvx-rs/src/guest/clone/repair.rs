//! The repair of a clone that a restore of a template's snapshot started, in the caller's
//! process: [`Repair`], and [`repair`], which runs one once, as the [module](super) describes.

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::mem;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use super::at;
use crate::guest::GenerationId;
use crate::guest::parse::{parse_decimal, parse_hexadecimal};
use crate::guest::reseed::{self, ReseedError, Seed};
use crate::guest::root::Paths;
use crate::guest::sys;
use crate::guest::time::{CpuSet, MemoryRange, Restore, RestoreFinish, TimeAbi, TimeError};
use crate::protocol::status;

/// The memory block size of the snapshot ABI, 128 MiB, to which a restore's memory ranges are
/// aligned.
const BLOCK_SIZE: u64 = 128 << 20;
/// The end of low RAM, where the MMIO aperture starts: 3 GiB.
const LOW_RAM_END: u64 = 3 << 30;
/// The start of high RAM, where the MMIO aperture ends: 4 GiB.
const HIGH_RAM_START: u64 = 4 << 30;
/// The directory of the kernel's memory blocks, below sysfs.
const MEMORY_BLOCKS: &str = "devices/system/memory";
/// The memory block size, which the kernel writes in hexadecimal, in the directory of the
/// memory blocks.
const BLOCK_SIZE_FILE: &str = "block_size_bytes";
/// The file to which a physical address is written to add the memory block that starts at it,
/// in the directory of the memory blocks.
const PROBE_FILE: &str = "probe";
/// The name of a memory block's directory, which the block's number follows, in the directory
/// of the memory blocks.
const BLOCK_PREFIX: &str = "memory";
/// The state of a memory block, such as `online` or `offline`, in the block's directory.
const STATE_FILE: &str = "state";
/// The directory of the kernel's CPUs, below sysfs.
const CPUS: &str = "devices/system/cpu";
/// The CPUs that are online, as a CPU list, below sysfs.
const ONLINE_CPUS: &str = "devices/system/cpu/online";
/// The name of a CPU's directory, which the CPU's number follows, in the directory of the CPUs.
const CPU_PREFIX: &str = "cpu";
/// The state of a CPU, `1` online and `0` offline, in the CPU's directory, to which `1` is
/// written to bring the CPU online.
const CPU_ONLINE_FILE: &str = "online";
/// The state of a memory block that is online.
const ONLINE: &[u8] = b"online";

/// Repairs the clone that `restore` started, once, as [`Repair::run`] does, and returns the
/// repaired clone.
///
/// The call takes the restore, so it can't be retried: a caller that retries a failed repair
/// keeps a [`Repair`] instead.
pub fn repair(restore: Restore) -> Result<Repaired, RepairError> {
    Repair::new(restore).run()
}

/// The repair of a clone that a restore started, of the [`Restore`] that the clone's capture
/// returned, as the [module](super) describes: its processor target, its memory target, its
/// reseed, and its finish, with the acknowledgement that the host awaits.
///
/// [`run`](Self::run) records how far the repair got, so that the repair never adds a memory
/// block twice, never reseeds twice, and never finishes the restore twice, whoever retries it:
///
/// - Until the memory target is active, a failed attempt may be retried, and the next one
///   repeats the checks, brings online only the CPUs of the processor target that are not online
///   yet, and adds or onlines only the memory blocks that are not online yet. The first attempt
///   records the CPUs that are online before it brings any online, and derives the end of the RAM
///   that the clone captured, before it adds any memory block, and every later one keeps both.
/// - The reseed and the finish run at most once. A failure of either is final: every later call
///   returns its error again.
/// - Once the repair completed, every later call returns the same [`Repaired`].
///
/// The repair takes the time ABI in strict mode, [`TimeAbi::strict`], so that a violation of the
/// ABI fails it, with the violation's status, instead of stopping the VM: it never powers the VM
/// off, and the caller ends the VM with [`RepairError::status`]. The repair never touches the
/// caller's fence of workload admission, which stays closed: the caller opens it when the clone
/// may admit executions, such as once it is activated.
pub struct Repair {
    /// The restore that the clone's capture returned, without its seed.
    restore: Restore,
    /// The restore's fresh entropy, until the reseed drops it.
    seed: Option<Seed>,
    /// Where the repair resumes.
    state: State,
    /// The number of CPUs that were online before the repair brought any online, a prefix of the
    /// CPUs, which its first attempt that reads them records.
    captured_cpus: Option<u32>,
    /// The memory blocks that the repair added and onlined, across its attempts.
    activations: Activations,
    /// How far the repair got, which its errors report.
    progress: RepairProgress,
    /// The kernel and the time ABI.
    system: Box<dyn RepairSystem>,
}

impl Repair {
    /// Returns the repair of the clone that `restore` started, which takes the restore's seed:
    /// the clone's capture requests the in-memory mode,
    /// [`CaptureRequest::in_memory`](crate::guest::time::CaptureRequest::in_memory), so that the
    /// restore holds it.
    pub fn new(restore: Restore) -> Self {
        let system = LiveSystem {
            paths: Paths::default(),
            abi: super::live_abi(),
        };
        Self::with_system(restore, Box::new(system))
    }

    /// Returns the repair of the clone that `restore` started, on `system`.
    fn with_system(mut restore: Restore, system: Box<dyn RepairSystem>) -> Self {
        let seed = restore.take_seed();
        Self {
            restore,
            seed,
            state: State::Memory { captured: None },
            captured_cpus: None,
            activations: Activations::default(),
            progress: RepairProgress::default(),
            system,
        }
    }

    /// Runs the repair from where it got, as the [module](super) describes, and returns the
    /// repaired clone.
    ///
    /// A failure names its [`RepairStage`], its [`RepairCause`], and the repair's
    /// [`RepairProgress`]. One before the reseed may be retried, and changes nothing that a retry
    /// undoes: a refused restore or processor target brings no CPU online and adds no memory
    /// block, a refused memory target adds no memory block, although the activation of the
    /// processor target before it may have brought CPUs online, and a failed activation leaves
    /// the CPUs that it brought online and the blocks that it added and onlined, which a retry
    /// counts as done. The reseed and the finish run at most once, so a failure of either, or of a
    /// finish that never returned, such as after a panic, is final, and every later call returns
    /// that error again. A failed finish may have acknowledged the restore already. Once the
    /// repair completed, every later call returns the same [`Repaired`], without another step.
    pub fn run(&mut self) -> Result<Repaired, RepairError> {
        self.resume().map_err(|error| RepairError {
            progress: self.progress,
            ..error
        })
    }

    /// Runs the repair from where it got, as [`run`](Self::run) does, with errors that do not
    /// tell its progress yet.
    fn resume(&mut self) -> Result<Repaired, RepairError> {
        loop {
            match &self.state {
                State::Memory { .. } => {
                    let resources = self.activate()?;
                    self.state = State::Reseed(resources);
                }
                State::Reseed(resources) => {
                    let resources = *resources;
                    if let Err(error) = self.reseed() {
                        self.state = State::Failed(error.clone());
                        return Err(error);
                    }
                    self.state = State::Finish(resources);
                }
                State::Finish(resources) => return self.finish(*resources),
                State::Repaired(repaired) => return Ok(*repaired),
                State::Failed(error) => return Err(error.clone()),
            }
        }
    }

    /// Checks the restore, and activates the processor and memory targets: the steps before the
    /// reseed, which a failure leaves to a retry.
    fn activate(&mut self) -> Result<Resources, RepairError> {
        if self.restore.finished() {
            return Err(RepairError::new(
                RepairStage::Restore,
                RepairCause::Finished,
            ));
        }
        if self.seed.is_none() {
            return Err(RepairError::new(RepairStage::Restore, RepairCause::NoSeed));
        }
        let (cpus, new_cpus) = self
            .activate_processors()
            .map_err(|cause| RepairError::new(RepairStage::Processors, cause))?;
        let memory = self
            .activate_memory()
            .map_err(|cause| RepairError::new(RepairStage::Memory, cause))?;
        Ok(Resources {
            cpus,
            new_cpus,
            memory,
        })
    }

    /// Activates the restore's processor target, if it has one, and returns the number of online
    /// CPUs, which must be a prefix of the CPUs, with the CPUs that the repair brought online.
    ///
    /// A target below the CPUs that are online is refused, since the repair takes no CPU
    /// offline. The repair brings online each CPU of the target's prefix that is offline, through
    /// sysfs, as `nvx-snapshot` does, then verifies that the online CPUs are the target's prefix.
    /// The CPUs that it brought online are those of the prefix beyond the ones that were online
    /// before its first attempt, which the clone captured.
    fn activate_processors(&mut self) -> Result<(u32, CpuSet), RepairCause> {
        let online = self.online_cpus()?;
        let captured = *self.captured_cpus.get_or_insert(online);
        let Some(target) = self.restore.processor_target() else {
            return Ok((online, CpuSet::new()));
        };
        let count = u32::from(target);
        if count < online {
            return Err(RepairCause::ProcessorTarget { target, online });
        }
        for cpu in online..count {
            self.system.online_cpu(cpu).map_err(RepairCause::Io)?;
        }
        let text = self.system.online_cpus().map_err(RepairCause::Io)?;
        if online_prefix(&text) != Some(count) {
            return Err(RepairCause::Activation {
                target,
                online: lossy(&text),
            });
        }
        // A processor target is at most 8 CPUs, which the time ABI covers.
        let new_cpus = (captured..count).fold(CpuSet::new(), |cpus, cpu| {
            cpus.with(cpu as usize).unwrap_or(cpus)
        });
        Ok((count, new_cpus))
    }

    /// Returns the number of online CPUs, which must be a prefix of the CPUs.
    fn online_cpus(&mut self) -> Result<u32, RepairCause> {
        let text = self.system.online_cpus().map_err(RepairCause::Io)?;
        online_prefix(&text).ok_or_else(|| RepairCause::OnlineCpus(lossy(&text)))
    }

    /// Activates the restore's memory target, if it has one, and returns the memory size of the
    /// online memory blocks.
    fn activate_memory(&mut self) -> Result<u64, RepairCause> {
        let ranges = !self.restore.memory_ranges().is_empty();
        if !self.restore.memory_target() && ranges {
            return Err(RepairCause::UntargetedRanges);
        }
        let text = self.system.block_size().map_err(RepairCause::Io)?;
        let block_size = parse_hexadecimal(trimmed(&text))
            .filter(|&size| size != 0 && (!self.restore.memory_target() || size == BLOCK_SIZE))
            .ok_or_else(|| RepairCause::BlockSize(lossy(&text)))?;
        if ranges {
            self.add_ranges()?;
        }
        self.online_memory(block_size)
    }

    /// Checks the restore's memory ranges against the RAM that the clone captured, before it adds
    /// any memory block, then adds each block of the ranges that the kernel lacks, onlines each
    /// that is not online, and verifies that it is.
    fn add_ranges(&mut self) -> Result<(), RepairCause> {
        let captured = match self.state {
            State::Memory {
                captured: Some(captured),
            } => captured,
            _ => {
                let blocks = self.system.memory_blocks().map_err(RepairCause::Io)?;
                let captured = captured_end(&blocks)?;
                self.state = State::Memory {
                    captured: Some(captured),
                };
                captured
            }
        };
        check_ranges(self.restore.memory_ranges(), captured)?;
        for range in self.restore.memory_ranges() {
            let mut address = range.start();
            while address < range.start() + range.length() {
                activate_block(
                    self.system.as_mut(),
                    &mut self.activations,
                    &mut self.progress,
                    address,
                )?;
                address += BLOCK_SIZE;
            }
        }
        Ok(())
    }

    /// Returns the memory size of the online memory blocks, of `block_size` bytes each.
    fn online_memory(&mut self, block_size: u64) -> Result<u64, RepairCause> {
        let mut online = 0_u64;
        for block in self.system.memory_blocks().map_err(RepairCause::Io)? {
            // A block that the kernel removed since it listed it is not online.
            if let Some(state) = self.system.block_state(block).map_err(RepairCause::Io)?
                && trimmed(&state) == ONLINE
            {
                online += 1;
            }
        }
        Ok(online.saturating_mul(block_size))
    }

    /// Reseeds the kernel's random number generator with the restore's seed, then drops the
    /// seed, which zeroes its bytes, whatever the reseed's outcome.
    fn reseed(&mut self) -> Result<(), RepairError> {
        let fail = |cause| RepairError::new(RepairStage::Reseed, cause);
        // A reseed that never returned, such as after a panic, dropped the seed already.
        let seed = self.seed.take().ok_or_else(|| fail(RepairCause::NoSeed))?;
        let result = self.system.reseed(&seed);
        drop(seed);
        result.map_err(|error| fail(RepairCause::Reseed(error)))?;
        self.progress.reseeded = true;
        Ok(())
    }

    /// Finishes the restore, with the acknowledgement if the host gates its input until then,
    /// once, and returns the repaired clone, which has `resources`. The finish records the CPUs
    /// that the repair brought online, which the time ABI's restore checks check too.
    fn finish(&mut self, resources: Resources) -> Result<Repaired, RepairError> {
        let acknowledge = self.restore.acknowledgement_required();
        let mut finish = RestoreFinish::new().new_cpus(resources.new_cpus);
        if acknowledge {
            finish = finish.acknowledge();
        }
        // The finish runs at most once, even when it never returns, such as after a panic.
        self.progress.finish_started = true;
        self.state = State::Failed(RepairError::new(
            RepairStage::Finish,
            RepairCause::Incomplete,
        ));
        // The finish acknowledges before any deferred work, which it may run in line.
        let acknowledged_at = acknowledge.then(Instant::now);
        if let Err(error) = self.system.restore_finish(finish) {
            let error = RepairError::new(RepairStage::Finish, RepairCause::Time(error));
            self.state = State::Failed(error.clone());
            return Err(error);
        }
        let repaired = Repaired {
            generation_id: self.restore.generation_id(),
            cpus: resources.cpus,
            memory: resources.memory,
            acknowledged_at,
        };
        self.state = State::Repaired(repaired);
        Ok(repaired)
    }
}

impl fmt::Debug for Repair {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Repair")
            .field("restore", &self.restore)
            .field("state", &self.state)
            .field("progress", &self.progress)
            .finish_non_exhaustive()
    }
}

/// Where a repair resumes, which its next attempt starts from.
#[derive(Debug)]
enum State {
    /// The memory target is not active yet, which the next attempt activates.
    Memory {
        /// The logical end of the RAM that the clone captured, which the first attempt that
        /// checks the memory ranges derives, before it adds any memory block.
        captured: Option<u64>,
    },
    /// The memory target is active, with these resources: the reseed is next.
    Reseed(Resources),
    /// The kernel's random number generator is reseeded: the finish is next.
    Finish(Resources),
    /// The repair completed.
    Repaired(Repaired),
    /// The repair failed at or after its reseed, which never runs again, nor does the finish.
    Failed(RepairError),
}

/// The resources that a clone has once its memory target is active.
#[derive(Clone, Copy, Debug)]
struct Resources {
    /// The number of online CPUs.
    cpus: u32,
    /// The CPUs that the repair brought online.
    new_cpus: CpuSet,
    /// The memory size of the online memory blocks, in bytes.
    memory: u64,
}

/// The memory blocks that a repair added and onlined, across its attempts, each with whether the
/// repair found it since, present or online, so that its progress counts each block once, once it
/// found it: an attempt may fail between an addition or an onlining that the kernel accepted and
/// the read that finds the block, which a later attempt then makes.
#[derive(Debug, Default)]
struct Activations {
    /// The blocks whose addition the kernel accepted, each with whether the repair found it since.
    added: BTreeMap<u64, bool>,
    /// The blocks whose onlining the kernel accepted, each with whether the repair found it online
    /// since.
    onlined: BTreeMap<u64, bool>,
}

/// Records that the repair found `block`, if `log` holds it, and returns whether it had not
/// found it before.
fn first_found(log: &mut BTreeMap<u64, bool>, block: u64) -> bool {
    log.get_mut(&block)
        .is_some_and(|found| !mem::replace(found, true))
}

/// A clone that its [`Repair`] repaired: its new VM generation ID, the resources that it has,
/// which an activation checks against the resources that it expects, and when its repair
/// acknowledged its restore.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Repaired {
    /// The new VM generation ID.
    generation_id: GenerationId,
    /// The number of online CPUs.
    cpus: u32,
    /// The memory size of the online memory blocks, in bytes.
    memory: u64,
    /// An instant no later than the repair's acknowledgement of the restore, taken right before
    /// the finish, if the restore asked for the acknowledgement.
    acknowledged_at: Option<Instant>,
}

impl Repaired {
    /// Returns the clone's new VM generation ID, with which the restore's seed started.
    pub fn generation_id(&self) -> GenerationId {
        self.generation_id
    }

    /// Returns the number of online CPUs, a prefix of the CPUs.
    pub fn cpus(&self) -> u32 {
        self.cpus
    }

    /// Returns the memory size, in bytes: the size of the memory blocks that are online, which
    /// is the restore's memory target when the restore has one.
    pub fn memory(&self) -> u64 {
        self.memory
    }

    /// Returns an instant no later than the repair's acknowledgement of the restore, or `None` if
    /// the host did not gate its input until the acknowledgement.
    ///
    /// The repair takes the instant right before its finish, which acknowledges the restore
    /// before any of the deferred time work, and may run that work before it returns, so a
    /// deadline that starts at the acknowledgement may count from the instant.
    pub fn acknowledged_at(&self) -> Option<Instant> {
        self.acknowledged_at
    }
}

/// How far a clone's repair got, across its attempts, which its [`RepairError`] reports: the
/// memory blocks that it added and brought online, whether it reseeded the kernel's random number
/// generator, and whether it started the finish, which may have acknowledged the restore.
///
/// It counts each memory block once, once the repair found it present after its addition, or
/// online after its onlining. A block whose addition or onlining the kernel accepted, but that an
/// attempt could not find, such as when a read of its state failed, counts once a later attempt
/// finds it.
///
/// Its `Display` form lists them, such as `added 2 memory blocks, onlined 2, reseeded, started
/// the finish`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct RepairProgress {
    /// The memory blocks that the repair added, and found since.
    added: u64,
    /// The memory blocks that the repair onlined, and found online since.
    onlined: u64,
    /// Whether the repair reseeded the kernel's random number generator.
    reseeded: bool,
    /// Whether the repair started the finish.
    finish_started: bool,
}

impl RepairProgress {
    /// Returns the number of memory blocks that the repair added through the kernel's memory
    /// probe interface, and found present since.
    pub fn added(&self) -> u64 {
        self.added
    }

    /// Returns the number of memory blocks that the repair brought online, and found online
    /// since, besides those that the kernel brought online itself.
    pub fn onlined(&self) -> u64 {
        self.onlined
    }

    /// Returns whether the repair reseeded the kernel's random number generator.
    pub fn reseeded(&self) -> bool {
        self.reseeded
    }

    /// Returns whether the repair started the restore's finish, which may have acknowledged the
    /// restore.
    pub fn finish_started(&self) -> bool {
        self.finish_started
    }
}

impl fmt::Display for RepairProgress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "added {} memory blocks, onlined {}",
            self.added, self.onlined
        )?;
        if self.reseeded {
            formatter.write_str(", reseeded")?;
        }
        if self.finish_started {
            formatter.write_str(", started the finish")?;
        }
        Ok(())
    }
}

/// A stage of a clone's repair, which a [`RepairError`] names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RepairStage {
    /// Checking that the restore holds its seed, and that its capture did not finish it.
    Restore,
    /// Checking the online CPUs, bringing those of the restore's processor target online, and
    /// verifying them.
    Processors,
    /// Checking the restore's memory target, adding and onlining its memory blocks, and
    /// verifying them.
    Memory,
    /// Reseeding the kernel's random number generator with the restore's seed.
    Reseed,
    /// Finishing the restore, with its acknowledgement, which hands the deferred time work to
    /// the time daemon.
    Finish,
}

impl fmt::Display for RepairStage {
    /// Writes what the stage does, such as `activate the memory target`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Restore => "check the restore",
            Self::Processors => "activate the processor target",
            Self::Memory => "activate the memory target",
            Self::Reseed => "reseed the kernel's random number generator",
            Self::Finish => "finish the restore",
        })
    }
}

/// What is wrong with a memory range of a restore, which a [`RepairCause::Range`] names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RangeFault {
    /// The range is empty, ends past the address space, or is not aligned to 128 MiB memory
    /// blocks.
    Misaligned,
    /// The range enters the MMIO aperture, between the end of low RAM at 3 GiB and the start of
    /// high RAM at 4 GiB.
    Aperture,
    /// The range is a second range of low RAM or of high RAM, or one of low RAM after one of
    /// high RAM.
    Extra,
    /// The range starts past the end of the RAM before it, in logical order, which leaves a gap.
    Gap,
    /// The range starts before the end of the RAM before it, in logical order, which it
    /// overlaps.
    Overlap,
}

impl fmt::Display for RangeFault {
    /// Writes what is wrong with the range, such as `overlaps the RAM before it`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Misaligned => "is not a nonempty range of whole 128 MiB memory blocks",
            Self::Aperture => "enters the MMIO aperture between 3 GiB and 4 GiB",
            Self::Extra => "is another range of low or high RAM",
            Self::Gap => "leaves a gap after the RAM before it",
            Self::Overlap => "overlaps the RAM before it",
        })
    }
}

/// Why a stage of a clone's repair failed, which a [`RepairError`] holds.
#[derive(Debug)]
#[non_exhaustive]
pub enum RepairCause {
    /// A read or write of sysfs failed with this error, which names the file.
    Io(io::Error),
    /// The restore holds no seed: its capture requested the file mode, or the seed was taken.
    NoSeed,
    /// The restore's capture finished it, so that the host may admit input before its repair.
    Finished,
    /// The online CPUs, as the kernel lists them, are not a prefix of the CPUs, such as `0-3`.
    OnlineCpus(String),
    /// The restore asks for fewer online CPUs than are online, which the repair doesn't take
    /// offline.
    ProcessorTarget {
        /// The number of online CPUs that the restore asks for.
        target: u8,
        /// The number of online CPUs.
        online: u32,
    },
    /// The online CPUs are not the processor target's prefix of the CPUs once the repair brought
    /// those of the target online.
    Activation {
        /// The number of online CPUs that the restore asks for.
        target: u8,
        /// The online CPUs, as the kernel lists them.
        online: String,
    },
    /// The kernel's memory block size, which it writes in hexadecimal, is not 128 MiB, as the
    /// restore's memory target needs, or is malformed.
    BlockSize(String),
    /// The memory blocks that the kernel had added before the activation are not the first ones
    /// of RAM in logical order, which a captured RAM holds.
    CapturedRam,
    /// The restore lists memory ranges without a memory target.
    UntargetedRanges,
    /// A memory range of the restore does not continue the RAM that the clone captured.
    Range {
        /// The range's index in the restore's list.
        index: usize,
        /// What is wrong with it.
        fault: RangeFault,
    },
    /// The kernel lacks this memory block after its addition.
    Missing(u64),
    /// A memory block is not online after its onlining.
    Offline {
        /// The block's number.
        block: u64,
        /// The block's state, as the kernel writes it.
        state: String,
    },
    /// The kernel refused the reseed.
    Reseed(ReseedError),
    /// The time ABI's finish failed: a violation that a strict or report-only call reports, whose
    /// status the error takes, or a failure of its ports or its time state.
    Time(TimeError),
    /// An earlier attempt at the stage, which runs at most once, never returned, such as after a
    /// panic.
    Incomplete,
}

impl fmt::Display for RepairCause {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => formatter.write_str(&sys::describe(error)),
            Self::NoSeed => formatter.write_str("the restore holds no seed"),
            Self::Finished => formatter.write_str("the restore's capture finished it"),
            Self::OnlineCpus(cpus) => {
                write!(
                    formatter,
                    "the online CPUs {cpus} are not a prefix of the CPUs"
                )
            }
            Self::ProcessorTarget { target, online } => write!(
                formatter,
                "the restore asks for {target} online CPUs, and {online} are online"
            ),
            Self::Activation { target, online } => write!(
                formatter,
                "the online CPUs are {online} once the first {target} were brought online"
            ),
            Self::BlockSize(size) => write!(
                formatter,
                "the memory block size is {size} in hexadecimal, not 128 MiB"
            ),
            Self::CapturedRam => {
                formatter.write_str("the memory blocks before the activation are not a prefix")
            }
            Self::UntargetedRanges => {
                formatter.write_str("the restore lists memory ranges without a memory target")
            }
            Self::Range { index, fault } => write!(formatter, "memory range {index} {fault}"),
            Self::Missing(block) => write!(formatter, "memory block {block} was not added"),
            Self::Offline { block, state } => {
                write!(formatter, "memory block {block} is {state}, not online")
            }
            Self::Reseed(error) => error.fmt(formatter),
            Self::Time(error) => error.fmt(formatter),
            Self::Incomplete => formatter.write_str("an earlier attempt never returned"),
        }
    }
}

/// Why a clone's repair failed: the stage that failed, its cause, and how far the repair got.
///
/// Its `Display` form names the stage and the cause, such as `failed to activate the memory
/// target: memory range 0 overlaps the RAM before it`. A clone keeps one error of a final
/// failure, which every later [`Repair::run`] returns again. The caller ends the clone's VM with
/// its [`status`](Self::status), after an error line that names it.
#[derive(Clone, Debug)]
pub struct RepairError {
    /// The stage that failed.
    stage: RepairStage,
    /// Why it failed.
    cause: Arc<RepairCause>,
    /// How far the repair got.
    progress: RepairProgress,
}

impl RepairError {
    /// Returns the failure of `stage`, because of `cause`, of a repair that did nothing yet.
    fn new(stage: RepairStage, cause: RepairCause) -> Self {
        Self {
            stage,
            cause: Arc::new(cause),
            progress: RepairProgress::default(),
        }
    }

    /// Returns the stage that failed.
    pub fn stage(&self) -> RepairStage {
        self.stage
    }

    /// Returns why it failed.
    pub fn cause(&self) -> &RepairCause {
        &self.cause
    }

    /// Returns how far the repair got, across its attempts, when it failed.
    pub fn progress(&self) -> RepairProgress {
        self.progress
    }

    /// Returns the exit status with which the clone's guest ends its VM for the failure, as
    /// `nvx::protocol::status` documents it. A violation of the time ABI, a [`RepairCause::Time`]
    /// of [`TimeError::Violation`], has the violation's
    /// [`status`](crate::guest::time::Violation::status): 193 for a conformance check, or 195
    /// for the time ABI's part of the repair. Any other failure has its stage's status, from 40
    /// to 45: [`REPAIR_RESTORE`](status::REPAIR_RESTORE),
    /// [`REPAIR_PROCESSORS`](status::REPAIR_PROCESSORS),
    /// [`REPAIR_MEMORY_TARGET`](status::REPAIR_MEMORY_TARGET) for a memory target that the memory
    /// stage refused before it added any memory block, [`REPAIR_MEMORY`](status::REPAIR_MEMORY)
    /// for the memory stage's other failures, [`REPAIR_RESEED`](status::REPAIR_RESEED), and
    /// [`REPAIR_FINISH`](status::REPAIR_FINISH).
    pub fn status(&self) -> u8 {
        if let RepairCause::Time(TimeError::Violation(violation)) = self.cause.as_ref() {
            return violation.status();
        }
        match (self.stage, self.cause.as_ref()) {
            (RepairStage::Restore, _) => status::REPAIR_RESTORE,
            (RepairStage::Processors, _) => status::REPAIR_PROCESSORS,
            (
                RepairStage::Memory,
                RepairCause::BlockSize(_)
                | RepairCause::CapturedRam
                | RepairCause::UntargetedRanges
                | RepairCause::Range { .. },
            ) => status::REPAIR_MEMORY_TARGET,
            (RepairStage::Memory, _) => status::REPAIR_MEMORY,
            (RepairStage::Reseed, _) => status::REPAIR_RESEED,
            (RepairStage::Finish, _) => status::REPAIR_FINISH,
        }
    }
}

impl fmt::Display for RepairError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "failed to {}: {}", self.stage, self.cause)
    }
}

impl std::error::Error for RepairError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self.cause.as_ref() {
            RepairCause::Io(error) => Some(error),
            RepairCause::Reseed(error) => Some(error),
            RepairCause::Time(error) => Some(error),
            _ => None,
        }
    }
}

/// Returns the number of online CPUs that the CPU list `text`, as the kernel writes it, names,
/// if they are a prefix of the CPUs, `0` or `0-N`, or `None`.
fn online_prefix(text: &[u8]) -> Option<u32> {
    let last = match trimmed(text) {
        b"0" => 0,
        range => parse_decimal(range.strip_prefix(b"0-")?)?,
    };
    u32::try_from(last).ok()?.checked_add(1)
}

/// Returns the logical end of the RAM that the memory blocks `blocks` hold, in ascending order,
/// which must be the first blocks of RAM in logical order: those of low RAM from address 0, then
/// those of high RAM from 4 GiB.
fn captured_end(blocks: &[u64]) -> Result<u64, RepairCause> {
    for (logical, &block) in (0_u64..).zip(blocks) {
        if block != physical_block(logical) {
            return Err(RepairCause::CapturedRam);
        }
    }
    Ok((blocks.len() as u64).saturating_mul(BLOCK_SIZE))
}

/// Returns the number of the memory block that holds the RAM's `logical`th block, in logical
/// order, which skips the MMIO aperture.
fn physical_block(logical: u64) -> u64 {
    /// The blocks of low RAM.
    const LOW_BLOCKS: u64 = LOW_RAM_END / BLOCK_SIZE;
    /// The blocks of the MMIO aperture.
    const APERTURE_BLOCKS: u64 = (HIGH_RAM_START - LOW_RAM_END) / BLOCK_SIZE;
    if logical < LOW_BLOCKS {
        logical
    } else {
        logical + APERTURE_BLOCKS
    }
}

/// Checks that `ranges` are the ranges that continue the RAM that the clone captured, whose
/// logical end is `captured`, as a restore's expansion of the RAM selects them: aligned to
/// memory blocks, in ascending order, with no gap or overlap in logical order, and at most one of
/// low RAM, which ends at 3 GiB, and one of high RAM, which starts at 4 GiB, so that none enters
/// the MMIO aperture between them.
fn check_ranges(ranges: &[MemoryRange], captured: u64) -> Result<(), RepairCause> {
    let mut next = captured;
    let (mut low, mut high) = (false, false);
    for (index, range) in ranges.iter().enumerate() {
        let fault = |fault| RepairCause::Range { index, fault };
        let (start, length) = (range.start(), range.length());
        let end = start
            .checked_add(length)
            .ok_or_else(|| fault(RangeFault::Misaligned))?;
        if length == 0 || !start.is_multiple_of(BLOCK_SIZE) || !length.is_multiple_of(BLOCK_SIZE) {
            return Err(fault(RangeFault::Misaligned));
        }
        let logical = if end <= LOW_RAM_END {
            if low || high {
                return Err(fault(RangeFault::Extra));
            }
            low = true;
            start
        } else if start >= HIGH_RAM_START {
            if high {
                return Err(fault(RangeFault::Extra));
            }
            high = true;
            start - (HIGH_RAM_START - LOW_RAM_END)
        } else {
            return Err(fault(RangeFault::Aperture));
        };
        if logical < next {
            return Err(fault(RangeFault::Overlap));
        }
        if logical > next {
            return Err(fault(RangeFault::Gap));
        }
        next = logical + length;
    }
    Ok(())
}

/// Adds the memory block that starts at `address` through `system`, unless the kernel has it
/// already, onlines it, unless it is online, and verifies that it is. `activations` records the
/// additions and onlinings that the kernel accepted, across the repair's attempts, and `progress`
/// counts the block once, once the repair found it present after its own addition, and once, once
/// it found it online after its own onlining.
fn activate_block(
    system: &mut dyn RepairSystem,
    activations: &mut Activations,
    progress: &mut RepairProgress,
    address: u64,
) -> Result<(), RepairCause> {
    let block = address / BLOCK_SIZE;
    let mut state = system.block_state(block).map_err(RepairCause::Io)?;
    if state.is_none() {
        system.add_block(address).map_err(RepairCause::Io)?;
        activations.added.entry(block).or_insert(false);
        state = system.block_state(block).map_err(RepairCause::Io)?;
    }
    let state = state.ok_or(RepairCause::Missing(block))?;
    if first_found(&mut activations.added, block) {
        progress.added += 1;
    }
    if trimmed(&state) != ONLINE {
        system.online_block(block).map_err(RepairCause::Io)?;
        activations.onlined.entry(block).or_insert(false);
        let state = system
            .block_state(block)
            .map_err(RepairCause::Io)?
            .ok_or(RepairCause::Missing(block))?;
        if trimmed(&state) != ONLINE {
            return Err(RepairCause::Offline {
                block,
                state: lossy(&state),
            });
        }
    }
    if first_found(&mut activations.onlined, block) {
        progress.onlined += 1;
    }
    Ok(())
}

/// Returns `text`, a value that the kernel writes in sysfs, without the newline that ends it.
fn trimmed(text: &[u8]) -> &[u8] {
    text.strip_suffix(b"\n").unwrap_or(text)
}

/// Returns `text`, a value that the kernel writes in sysfs, without its newline, as a string for
/// a diagnostic.
fn lossy(text: &[u8]) -> String {
    String::from_utf8_lossy(trimmed(text)).into_owned()
}

/// What a repair reads and writes of the kernel, and asks of the time ABI, which the tests' fake
/// replaces.
trait RepairSystem: Send {
    /// Reads the CPUs that are online, as the kernel lists them.
    fn online_cpus(&mut self) -> io::Result<Vec<u8>>;

    /// Brings the CPU `cpu` online.
    fn online_cpu(&mut self, cpu: u32) -> io::Result<()>;

    /// Reads the memory block size, which the kernel writes in hexadecimal.
    fn block_size(&mut self) -> io::Result<Vec<u8>>;

    /// Returns the numbers of the memory blocks that the kernel added, in ascending order.
    fn memory_blocks(&mut self) -> io::Result<Vec<u64>>;

    /// Reads the state of the memory block `block`, or returns `None` if the kernel lacks it.
    fn block_state(&mut self, block: u64) -> io::Result<Option<Vec<u8>>>;

    /// Adds the memory block that starts at the physical address `address`.
    fn add_block(&mut self, address: u64) -> io::Result<()>;

    /// Onlines the memory block `block`.
    fn online_block(&mut self, block: u64) -> io::Result<()>;

    /// Reseeds the kernel's random number generator with `seed`, as [`reseed::reseed`] does.
    fn reseed(&mut self, seed: &Seed) -> Result<(), ReseedError>;

    /// Finishes the restore, as [`TimeAbi::restore_finish`] does.
    fn restore_finish(&mut self, finish: RestoreFinish) -> Result<(), TimeError>;
}

/// The guest's kernel, at the paths of its interfaces, and its time ABI.
struct LiveSystem {
    /// The directories of the kernel's interfaces.
    paths: Paths,
    /// The time ABI, in strict mode unless the kernel command line makes it report-only.
    abi: TimeAbi,
}

impl LiveSystem {
    /// Returns the path of `name` in the directory of the memory blocks.
    fn memory(&self, name: &str) -> PathBuf {
        self.paths.sys.join(MEMORY_BLOCKS).join(name)
    }

    /// Returns the path of the state of the memory block `block`.
    fn state(&self, block: u64) -> PathBuf {
        self.memory(&format!("{BLOCK_PREFIX}{block}"))
            .join(STATE_FILE)
    }
}

impl RepairSystem for LiveSystem {
    fn online_cpus(&mut self) -> io::Result<Vec<u8>> {
        read(&self.paths.sys.join(ONLINE_CPUS))
    }

    fn online_cpu(&mut self, cpu: u32) -> io::Result<()> {
        let path = self
            .paths
            .sys
            .join(CPUS)
            .join(format!("{CPU_PREFIX}{cpu}"))
            .join(CPU_ONLINE_FILE);
        write(&path, b"1\n")
    }

    fn block_size(&mut self) -> io::Result<Vec<u8>> {
        read(&self.memory(BLOCK_SIZE_FILE))
    }

    fn memory_blocks(&mut self) -> io::Result<Vec<u64>> {
        let directory = self.paths.sys.join(MEMORY_BLOCKS);
        let mut blocks = Vec::new();
        for entry in fs::read_dir(&directory).map_err(|error| at(&directory, &error))? {
            let entry = entry.map_err(|error| at(&directory, &error))?;
            if let Some(block) = entry
                .file_name()
                .as_bytes()
                .strip_prefix(BLOCK_PREFIX.as_bytes())
                .and_then(parse_decimal)
            {
                blocks.push(block);
            }
        }
        blocks.sort_unstable();
        Ok(blocks)
    }

    fn block_state(&mut self, block: u64) -> io::Result<Option<Vec<u8>>> {
        let path = self.state(block);
        match fs::read(&path) {
            Ok(state) => Ok(Some(state)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(at(&path, &error)),
        }
    }

    fn add_block(&mut self, address: u64) -> io::Result<()> {
        write(
            &self.memory(PROBE_FILE),
            format!("{address:#x}\n").as_bytes(),
        )
    }

    fn online_block(&mut self, block: u64) -> io::Result<()> {
        write(&self.state(block), b"online\n")
    }

    fn reseed(&mut self, seed: &Seed) -> Result<(), ReseedError> {
        reseed::reseed(seed)
    }

    fn restore_finish(&mut self, finish: RestoreFinish) -> Result<(), TimeError> {
        self.abi.restore_finish(finish)
    }
}

/// Reads the file at `path`, with errors that name it.
fn read(path: &Path) -> io::Result<Vec<u8>> {
    fs::read(path).map_err(|error| at(path, &error))
}

/// Writes `bytes` to the file at `path`, which must exist, such as a sysfs attribute, with errors
/// that name it.
fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|mut file| file.write_all(bytes))
        .map_err(|error| at(path, &error))
}
