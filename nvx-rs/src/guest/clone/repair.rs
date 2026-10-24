//! The repair of a clone that a restore of a template's snapshot started, in the caller's
//! process: [`Repair`], and [`repair`], which runs one once, as the [module](super) describes.

#[cfg(test)]
mod tests;

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
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
use crate::guest::time::{MemoryRange, Restore, RestoreFinish, TimeAbi, TimeError};

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
/// The CPUs that are online, as a CPU list, below sysfs.
const ONLINE_CPUS: &str = "devices/system/cpu/online";
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
/// returned, as the [module](super) describes: its memory target, its reseed, and its finish,
/// with the acknowledgement that the host awaits.
///
/// [`run`](Self::run) records how far the repair got, so that the repair never adds a memory
/// block twice, never reseeds twice, and never finishes the restore twice, whoever retries it:
///
/// - Until the memory target is active, a failed attempt may be retried, and the next one
///   repeats the checks and adds or onlines only the memory blocks that are not online yet. The
///   first attempt derives the end of the RAM that the clone captured, before it adds any block,
///   and every later one keeps it.
/// - The reseed and the finish run at most once. A failure of either is final: every later call
///   returns its error again.
/// - Once the repair completed, every later call returns the same [`Repaired`].
///
/// The repair never touches the caller's fence of workload admission, which stays closed: the
/// caller opens it when the clone may admit executions, such as once it is activated.
pub struct Repair {
    /// The restore that the clone's capture returned, without its seed.
    restore: Restore,
    /// The restore's fresh entropy, until the reseed drops it.
    seed: Option<Seed>,
    /// How far the repair got.
    progress: Progress,
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
            abi: TimeAbi::new(),
        };
        Self::with_system(restore, Box::new(system))
    }

    /// Returns the repair of the clone that `restore` started, on `system`.
    fn with_system(mut restore: Restore, system: Box<dyn RepairSystem>) -> Self {
        let seed = restore.take_seed();
        Self {
            restore,
            seed,
            progress: Progress::Memory { captured: None },
            system,
        }
    }

    /// Runs the repair from where it got, as the [module](super) describes, and returns the
    /// repaired clone.
    ///
    /// A failure names its [`RepairStage`] and its [`RepairCause`]. One before the reseed may be
    /// retried, and changes nothing that a retry undoes: a refused restore or memory target adds
    /// no memory block, and a failed activation leaves the blocks that it added and onlined,
    /// which a retry counts as done. The reseed and the finish run at most once, so a failure of
    /// either, or of a finish that never returned, such as after a panic, is final, and every
    /// later call returns that error again. A failed finish may have acknowledged the restore
    /// already. Once the repair completed, every later call returns the same [`Repaired`],
    /// without another step.
    pub fn run(&mut self) -> Result<Repaired, RepairError> {
        loop {
            match &self.progress {
                Progress::Memory { .. } => {
                    let resources = self.activate()?;
                    self.progress = Progress::Reseed(resources);
                }
                Progress::Reseed(resources) => {
                    let resources = *resources;
                    if let Err(error) = self.reseed() {
                        self.progress = Progress::Failed(error.clone());
                        return Err(error);
                    }
                    self.progress = Progress::Finish(resources);
                }
                Progress::Finish(resources) => return self.finish(*resources),
                Progress::Repaired(repaired) => return Ok(*repaired),
                Progress::Failed(error) => return Err(error.clone()),
            }
        }
    }

    /// Checks the restore and the online CPUs, and activates the memory target: the steps before
    /// the reseed, which a failure leaves to a retry.
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
        let cpus = self
            .check_processors()
            .map_err(|cause| RepairError::new(RepairStage::Processors, cause))?;
        let memory = self
            .activate_memory()
            .map_err(|cause| RepairError::new(RepairStage::Memory, cause))?;
        Ok(Resources { cpus, memory })
    }

    /// Returns the number of online CPUs, which must be a prefix of the CPUs, and must meet the
    /// restore's processor target, if it has one, since the repair brings no CPU online.
    fn check_processors(&mut self) -> Result<u32, RepairCause> {
        let text = self.system.online_cpus().map_err(RepairCause::Io)?;
        let online = online_prefix(&text).ok_or_else(|| RepairCause::OnlineCpus(lossy(&text)))?;
        if let Some(target) = self.restore.processor_target()
            && u32::from(target) != online
        {
            return Err(RepairCause::ProcessorTarget { target, online });
        }
        Ok(online)
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
        let captured = match self.progress {
            Progress::Memory {
                captured: Some(captured),
            } => captured,
            _ => {
                let blocks = self.system.memory_blocks().map_err(RepairCause::Io)?;
                let captured = captured_end(&blocks)?;
                self.progress = Progress::Memory {
                    captured: Some(captured),
                };
                captured
            }
        };
        check_ranges(self.restore.memory_ranges(), captured)?;
        for range in self.restore.memory_ranges() {
            let mut address = range.start();
            while address < range.start() + range.length() {
                activate_block(self.system.as_mut(), address)?;
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
        result.map_err(|error| fail(RepairCause::Reseed(error)))
    }

    /// Finishes the restore, with the acknowledgement if the host gates its input until then,
    /// once, and returns the repaired clone, which has `resources`.
    fn finish(&mut self, resources: Resources) -> Result<Repaired, RepairError> {
        let acknowledge = self.restore.acknowledgement_required();
        let mut finish = RestoreFinish::new();
        if acknowledge {
            finish = finish.acknowledge();
        }
        // The finish runs at most once, even when it never returns, such as after a panic.
        self.progress = Progress::Failed(RepairError::new(
            RepairStage::Finish,
            RepairCause::Incomplete,
        ));
        if let Err(error) = self.system.restore_finish(finish) {
            let error = RepairError::new(RepairStage::Finish, RepairCause::Time(error));
            self.progress = Progress::Failed(error.clone());
            return Err(error);
        }
        let repaired = Repaired {
            generation_id: self.restore.generation_id(),
            cpus: resources.cpus,
            memory: resources.memory,
            acknowledged_at: acknowledge.then(Instant::now),
        };
        self.progress = Progress::Repaired(repaired);
        Ok(repaired)
    }
}

impl fmt::Debug for Repair {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Repair")
            .field("restore", &self.restore)
            .field("progress", &self.progress)
            .finish_non_exhaustive()
    }
}

/// How far a repair got, from which its next attempt resumes.
#[derive(Debug)]
enum Progress {
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
    /// The memory size of the online memory blocks, in bytes.
    memory: u64,
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
    /// When the repair acknowledged the restore, if the restore asked for it.
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

    /// Returns when the repair acknowledged the restore, once its finish returned, or `None` if
    /// the host did not gate its input until the acknowledgement.
    pub fn acknowledged_at(&self) -> Option<Instant> {
        self.acknowledged_at
    }
}

/// A stage of a clone's repair, which a [`RepairError`] names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RepairStage {
    /// Checking that the restore holds its seed, and that its capture did not finish it.
    Restore,
    /// Checking the online CPUs against the restore's processor target.
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
            Self::Processors => "check the online CPUs",
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
    /// The restore asks for another number of online CPUs, which needs CPUs brought online,
    /// which the repair doesn't do.
    ProcessorTarget {
        /// The number of online CPUs that the restore asks for.
        target: u8,
        /// The number of online CPUs.
        online: u32,
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
    /// The time ABI's finish failed: a violation that a report-only call reports, or a failure of
    /// its ports or its time state.
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

/// Why a clone's repair failed: the stage that failed, and its cause.
///
/// Its `Display` form names both, such as `failed to activate the memory target: memory range 0
/// overlaps the RAM before it`. A clone keeps one error of a final failure, which every later
/// [`Repair::run`] returns again.
#[derive(Clone, Debug)]
pub struct RepairError {
    /// The stage that failed.
    stage: RepairStage,
    /// Why it failed.
    cause: Arc<RepairCause>,
}

impl RepairError {
    /// Returns the failure of `stage`, because of `cause`.
    fn new(stage: RepairStage, cause: RepairCause) -> Self {
        Self {
            stage,
            cause: Arc::new(cause),
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
/// already, onlines it, unless it is online, and verifies that it is.
fn activate_block(system: &mut dyn RepairSystem, address: u64) -> Result<(), RepairCause> {
    let block = address / BLOCK_SIZE;
    let state = match system.block_state(block).map_err(RepairCause::Io)? {
        Some(state) => state,
        None => {
            system.add_block(address).map_err(RepairCause::Io)?;
            system
                .block_state(block)
                .map_err(RepairCause::Io)?
                .ok_or(RepairCause::Missing(block))?
        }
    };
    if trimmed(&state) == ONLINE {
        return Ok(());
    }
    system.online_block(block).map_err(RepairCause::Io)?;
    let state = system
        .block_state(block)
        .map_err(RepairCause::Io)?
        .ok_or(RepairCause::Missing(block))?;
    if trimmed(&state) == ONLINE {
        Ok(())
    } else {
        Err(RepairCause::Offline {
            block,
            state: lossy(&state),
        })
    }
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
    /// The time ABI, in the mode that the kernel command line selects.
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
