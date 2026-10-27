//! Tests of a clone's repair, with a fake system whose CPUs, memory blocks, reseed, and time ABI
//! each test chooses, and which records its calls in one log, and of the live system's reads and
//! writes of sysfs, in a temporary directory.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread;
use std::time::Duration;

use super::*;
use crate::guest::reseed::tests::take_dropped;
use crate::guest::sys::Errno;
use crate::guest::time::Violation;

/// The new VM generation ID of the clone.
const RESTORED: GenerationId = GenerationId::from_bytes([8; GenerationId::LENGTH]);
/// One mebibyte.
const MIB: u64 = 1 << 20;
/// One gibibyte.
const GIB: u64 = 1 << 30;
/// The state of a memory block that is online, as the kernel writes it.
const ONLINE_STATE: &[u8] = b"online\n";
/// The state of a memory block that is offline, as the kernel writes it.
const OFFLINE_STATE: &[u8] = b"offline\n";

/// A call of a test's fake system.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    /// The online CPUs were read.
    OnlineCpus,
    /// The CPU of this number was brought online.
    OnlineCpu(u32),
    /// The memory block size was read.
    BlockSize,
    /// The memory blocks were listed.
    MemoryBlocks,
    /// The memory block at this physical address was added.
    Add(u64),
    /// The memory block of this number was onlined.
    Online(u64),
    /// The kernel was reseeded with the seed of this generation ID.
    Reseed(GenerationId),
    /// The bytes of the seeds that the thread dropped since the repair started, as their drops
    /// left them, which the finish records before it runs.
    Dropped(Vec<[u8; Seed::LENGTH]>),
    /// The restore was finished.
    Finish(RestoreFinish),
}

/// The log of a test's calls, which the test shares with its fake system.
#[derive(Clone, Default)]
struct Calls(Arc<Mutex<Vec<Call>>>);

impl Calls {
    /// Records `call`.
    fn push(&self, call: Call) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(call);
    }

    /// Returns the calls recorded since the last take, in order.
    fn take(&self) -> Vec<Call> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// A system that records its calls, except its reads of the blocks' states, with CPUs, memory
/// blocks, a reseed, and a time ABI that a test chooses: by default, two online CPUs, 128 MiB
/// memory blocks, the four online blocks of a clone that captured 512 MiB of RAM, and a reseed
/// and a finish that succeed. Bringing online the CPU that follows a prefix of online CPUs
/// extends the prefix. It opens no file and draws no randomness: it keeps its blocks in ordered
/// maps, which draw no random keys, as hashed maps would.
struct FakeSystem {
    /// The log.
    calls: Calls,
    /// The online CPUs, as the kernel lists them.
    cpus: Vec<u8>,
    /// The error numbers with which the next onlining of each CPU fails, once.
    cpu_failures: BTreeMap<u32, Errno>,
    /// A CPU that the kernel leaves offline when it is brought online.
    stuck_cpu: Option<u32>,
    /// The memory block size, as the kernel writes it.
    block_size: &'static [u8],
    /// The state of each memory block that the kernel has, by the block's number.
    blocks: BTreeMap<u64, &'static [u8]>,
    /// The error numbers with which the next addition of the block at each physical address
    /// fails, once.
    add_failures: BTreeMap<u64, Errno>,
    /// The error numbers with which the next onlining of each block fails, once.
    online_failures: BTreeMap<u64, Errno>,
    /// The error numbers with which a read of a block's state fails, once, by the block's number
    /// and the read's index among the reads of its state, from 0.
    read_failures: BTreeMap<(u64, usize), Errno>,
    /// The number of reads of each block's state so far.
    reads: BTreeMap<u64, usize>,
    /// Whether an added block comes online by itself, as the kernel onlines added memory when its
    /// policy says so.
    auto_online: bool,
    /// A block that the kernel leaves offline when it is onlined.
    stuck: Option<u64>,
    /// The error number with which the reseed fails, if any.
    reseed_failure: Option<Errno>,
    /// Whether the finish fails.
    finish_fails: bool,
    /// The violation with which the finish fails, as the strict time ABI's finish does, if any.
    finish_violation: Option<Violation>,
    /// Whether the finish panics, as one that never returns would leave the repair.
    finish_panics: bool,
}

impl FakeSystem {
    /// Returns the default system, which records its calls in `calls`.
    fn new(calls: &Calls) -> Self {
        Self {
            calls: calls.clone(),
            cpus: b"0-1\n".to_vec(),
            cpu_failures: BTreeMap::new(),
            stuck_cpu: None,
            block_size: b"8000000\n",
            blocks: (0..4).map(|block| (block, ONLINE_STATE)).collect(),
            add_failures: BTreeMap::new(),
            online_failures: BTreeMap::new(),
            read_failures: BTreeMap::new(),
            reads: BTreeMap::new(),
            auto_online: false,
            stuck: None,
            reseed_failure: None,
            finish_fails: false,
            finish_violation: None,
            finish_panics: false,
        }
    }

    /// Returns the system whose kernel has the first `count` memory blocks of RAM, in logical
    /// order, online instead.
    fn capturing(mut self, count: u64) -> Self {
        self.blocks = (0..count)
            .map(|logical| (physical_block(logical), ONLINE_STATE))
            .collect();
        self
    }
}

impl RepairSystem for FakeSystem {
    fn online_cpus(&mut self) -> io::Result<Vec<u8>> {
        self.calls.push(Call::OnlineCpus);
        Ok(self.cpus.clone())
    }

    fn online_cpu(&mut self, cpu: u32) -> io::Result<()> {
        self.calls.push(Call::OnlineCpu(cpu));
        if let Some(errno) = self.cpu_failures.remove(&cpu) {
            return Err(errno.into());
        }
        if self.stuck_cpu != Some(cpu) && online_prefix(&self.cpus) == Some(cpu) {
            self.cpus = format!("0-{cpu}\n").into_bytes();
        }
        Ok(())
    }

    fn block_size(&mut self) -> io::Result<Vec<u8>> {
        self.calls.push(Call::BlockSize);
        Ok(self.block_size.to_vec())
    }

    fn memory_blocks(&mut self) -> io::Result<Vec<u64>> {
        self.calls.push(Call::MemoryBlocks);
        Ok(self.blocks.keys().copied().collect())
    }

    fn block_state(&mut self, block: u64) -> io::Result<Option<Vec<u8>>> {
        let reads = self.reads.entry(block).or_default();
        let read = *reads;
        *reads += 1;
        if let Some(errno) = self.read_failures.remove(&(block, read)) {
            return Err(errno.into());
        }
        Ok(self.blocks.get(&block).map(|state| state.to_vec()))
    }

    fn add_block(&mut self, address: u64) -> io::Result<()> {
        self.calls.push(Call::Add(address));
        if let Some(errno) = self.add_failures.remove(&address) {
            return Err(errno.into());
        }
        let block = address / BLOCK_SIZE;
        // The kernel refuses to add a block that it has.
        if self.blocks.contains_key(&block) {
            return Err(Errno::EEXIST.into());
        }
        let state = if self.auto_online {
            ONLINE_STATE
        } else {
            OFFLINE_STATE
        };
        self.blocks.insert(block, state);
        Ok(())
    }

    fn online_block(&mut self, block: u64) -> io::Result<()> {
        self.calls.push(Call::Online(block));
        if let Some(errno) = self.online_failures.remove(&block) {
            return Err(errno.into());
        }
        let state = self.blocks.get_mut(&block).ok_or(Errno::ENOENT)?;
        if self.stuck != Some(block) {
            *state = ONLINE_STATE;
        }
        Ok(())
    }

    fn reseed(&mut self, seed: &Seed) -> Result<(), ReseedError> {
        self.calls.push(Call::Reseed(seed.generation_id()));
        match self.reseed_failure {
            Some(errno) => Err(ReseedError::Reseed(errno.into())),
            None => Ok(()),
        }
    }

    fn restore_finish(&mut self, finish: RestoreFinish) -> Result<(), TimeError> {
        self.calls.push(Call::Dropped(take_dropped()));
        self.calls.push(Call::Finish(finish));
        if self.finish_panics {
            panic!("the finish never returns");
        }
        if let Some(violation) = &self.finish_violation {
            return Err(TimeError::Violation(violation.clone()));
        }
        if self.finish_fails {
            Err(TimeError::StateUnavailable)
        } else {
            Ok(())
        }
    }
}

/// Returns the seed of the clone, whose bytes start with its new VM generation ID.
fn seed() -> Seed {
    let mut bytes = [0x5a; Seed::LENGTH];
    bytes[..GenerationId::LENGTH].copy_from_slice(&RESTORED.to_bytes());
    Seed::from_bytes(bytes)
}

/// Returns the restore of the clone, which holds the clone's seed, and whose host gates its input
/// until the clone acknowledges it, with nothing to activate.
fn restore() -> Restore {
    Restore::to(RESTORED).holding(seed()).gated()
}

/// Returns the repair of `restore` on `system`, once the test's thread forgot the seeds that it
/// dropped before.
fn repairing(restore: Restore, system: FakeSystem) -> Repair {
    take_dropped();
    Repair::with_system(restore, Box::new(system))
}

/// Returns the calls of a repair once its memory target is active: the listing of the memory
/// blocks, whose online ones it counts, the reseed, the drop of the seed, which zeroed it before
/// the finish, and the finish, with the acknowledgement.
fn finishing() -> [Call; 4] {
    [
        Call::MemoryBlocks,
        Call::Reseed(RESTORED),
        Call::Dropped(vec![[0; Seed::LENGTH]]),
        Call::Finish(RestoreFinish::new().acknowledge()),
    ]
}

/// Returns the addition and the onlining of each of the memory blocks `blocks`, in order.
fn activations(blocks: impl IntoIterator<Item = u64>) -> Vec<Call> {
    blocks
        .into_iter()
        .flat_map(|block| [Call::Add(block * BLOCK_SIZE), Call::Online(block)])
        .collect()
}

/// Returns `calls` without the reads of the CPUs and of the block size, which each attempt
/// repeats.
fn changes(calls: Vec<Call>) -> Vec<Call> {
    calls
        .into_iter()
        .filter(|call| !matches!(call, Call::OnlineCpus | Call::BlockSize))
        .collect()
}

/// Returns the set of `cpus`.
fn cpu_set(cpus: impl IntoIterator<Item = usize>) -> CpuSet {
    cpus.into_iter()
        .fold(CpuSet::new(), |set, cpu| set.with(cpu).unwrap())
}

#[test]
fn a_clone_is_reseeded_then_acknowledged_once_and_reentry_returns_the_same_clone() {
    let calls = Calls::default();
    let mut repair = repairing(restore(), FakeSystem::new(&calls));
    let repaired = repair.run().unwrap();
    assert_eq!(repaired.generation_id(), RESTORED);
    assert_eq!(repaired.cpus(), 2);
    assert_eq!(repaired.memory(), 512 * MIB);
    assert!(repaired.acknowledged_at().is_some());
    // The repair dropped the seed, which zeroed it, after the reseed and before the finish.
    let mut expected = vec![Call::OnlineCpus, Call::BlockSize];
    expected.extend(finishing());
    assert_eq!(calls.take(), expected);
    // Entered again, the repair returns the same clone, without another step.
    assert_eq!(repair.run().unwrap(), repaired);
    assert_eq!(repair.run().unwrap(), repaired);
    assert_eq!(calls.take(), Vec::<Call>::new());
    assert_eq!(take_dropped(), Vec::<[u8; Seed::LENGTH]>::new());
}

#[test]
fn an_ungated_restore_is_finished_without_an_acknowledgement() {
    let calls = Calls::default();
    let restore = Restore::to(RESTORED).holding(seed());
    let repaired = repairing(restore, FakeSystem::new(&calls)).run().unwrap();
    assert_eq!(repaired.acknowledged_at(), None);
    assert_eq!(
        calls.take().last(),
        Some(&Call::Finish(RestoreFinish::new()))
    );
}

/// How long the finish of a [`Deferring`] system runs the restore's deferred work after its
/// acknowledgement: long enough that an instant taken once the finish returned would follow the
/// acknowledgement by far.
const DEFERRED_WORK: Duration = Duration::from_millis(50);

/// A system that makes its calls through a [`FakeSystem`], and whose finish acknowledges the
/// restore, then runs the restore's deferred work for [`DEFERRED_WORK`] before it returns, as the
/// time ABI's finish does without a time daemon. It records when its reseed, the step before the
/// finish, returned, and when its finish acknowledged the restore.
struct Deferring {
    /// The system through which it makes its calls.
    system: FakeSystem,
    /// When the reseed returned.
    reseeded: Arc<OnceLock<Instant>>,
    /// When the finish acknowledged the restore.
    acknowledged: Arc<OnceLock<Instant>>,
}

impl RepairSystem for Deferring {
    fn online_cpus(&mut self) -> io::Result<Vec<u8>> {
        self.system.online_cpus()
    }

    fn online_cpu(&mut self, cpu: u32) -> io::Result<()> {
        self.system.online_cpu(cpu)
    }

    fn block_size(&mut self) -> io::Result<Vec<u8>> {
        self.system.block_size()
    }

    fn memory_blocks(&mut self) -> io::Result<Vec<u64>> {
        self.system.memory_blocks()
    }

    fn block_state(&mut self, block: u64) -> io::Result<Option<Vec<u8>>> {
        self.system.block_state(block)
    }

    fn add_block(&mut self, address: u64) -> io::Result<()> {
        self.system.add_block(address)
    }

    fn online_block(&mut self, block: u64) -> io::Result<()> {
        self.system.online_block(block)
    }

    fn reseed(&mut self, seed: &Seed) -> Result<(), ReseedError> {
        let result = self.system.reseed(seed);
        self.reseeded.set(Instant::now()).unwrap();
        result
    }

    fn restore_finish(&mut self, finish: RestoreFinish) -> Result<(), TimeError> {
        self.system.restore_finish(finish)?;
        self.acknowledged.set(Instant::now()).unwrap();
        thread::sleep(DEFERRED_WORK);
        Ok(())
    }
}

#[test]
fn the_instant_of_the_acknowledgement_precedes_the_deferred_work_of_the_finish() {
    let calls = Calls::default();
    let reseeded = Arc::new(OnceLock::new());
    let acknowledged = Arc::new(OnceLock::new());
    let system = Deferring {
        system: FakeSystem::new(&calls),
        reseeded: Arc::clone(&reseeded),
        acknowledged: Arc::clone(&acknowledged),
    };
    let restore = restore();
    take_dropped();
    let repaired = Repair::with_system(restore, Box::new(system))
        .run()
        .unwrap();
    let returned = Instant::now();
    let reseeded = *reseeded.get().unwrap();
    let acknowledged = *acknowledged.get().unwrap();
    // The finish returned long after it acknowledged the restore, but the repair took its instant
    // between the reseed and the acknowledgement, so that a deadline that starts at the instant
    // starts no later than at the acknowledgement.
    assert!(returned.duration_since(acknowledged) >= DEFERRED_WORK);
    let instant = repaired.acknowledged_at().unwrap();
    assert!(
        reseeded <= instant,
        "{instant:?} precedes the reseed, {reseeded:?}"
    );
    assert!(
        instant <= acknowledged,
        "{instant:?} follows the acknowledgement, {acknowledged:?}"
    );
    let mut expected = vec![Call::OnlineCpus, Call::BlockSize];
    expected.extend(finishing());
    assert_eq!(calls.take(), expected);
}

/// Checks that the repair of a clone that captured the first `captured` memory blocks of RAM, in
/// logical order, and whose restore's memory target has the expansion ranges `ranges`, adds and
/// onlines the blocks `added`, in order, after it listed the captured ones, and leaves the clone
/// with `memory` bytes of memory.
fn expands(captured: u64, ranges: &[(u64, u64)], added: &[u64], memory: u64) {
    let calls = Calls::default();
    let system = FakeSystem::new(&calls).capturing(captured);
    let repaired = repairing(restore().with_memory_target(ranges), system)
        .run()
        .unwrap();
    assert_eq!(repaired.memory(), memory, "{ranges:?}");
    let mut expected = vec![Call::OnlineCpus, Call::BlockSize];
    if !ranges.is_empty() {
        // The listing of the captured blocks, before any addition.
        expected.push(Call::MemoryBlocks);
    }
    expected.extend(activations(added.iter().copied()));
    expected.extend(finishing());
    assert_eq!(calls.take(), expected, "{ranges:?}");
}

#[test]
fn a_memory_target_adds_and_onlines_the_blocks_that_continue_the_captured_ram() {
    // Low RAM, continued.
    expands(4, &[(512 * MIB, 512 * MIB)], &[4, 5, 6, 7], GIB);
    // Low RAM, continued to its end at 3 GiB, then high RAM from 4 GiB.
    expands(
        20,
        &[(2560 * MIB, 512 * MIB), (4 * GIB, 512 * MIB)],
        &[20, 21, 22, 23, 32, 33, 34, 35],
        3584 * MIB,
    );
    // High RAM, after all of low RAM.
    expands(24, &[(4 * GIB, 256 * MIB)], &[32, 33], 3328 * MIB);
    // High RAM, continued.
    expands(28, &[(4608 * MIB, 128 * MIB)], &[36], 3712 * MIB);
    // An explicit target without ranges, which the RAM meets.
    expands(24, &[], &[], 3 * GIB);
}

#[test]
fn blocks_that_the_kernel_onlines_itself_are_only_added() {
    let calls = Calls::default();
    let mut system = FakeSystem::new(&calls);
    system.auto_online = true;
    let restore = restore().with_memory_target(&[(512 * MIB, 256 * MIB)]);
    let repaired = repairing(restore, system).run().unwrap();
    assert_eq!(repaired.memory(), 768 * MIB);
    let mut expected = vec![
        Call::MemoryBlocks,
        Call::Add(512 * MIB),
        Call::Add(640 * MIB),
    ];
    expected.extend(finishing());
    assert_eq!(changes(calls.take()), expected);
}

/// Checks that the repair of a clone that captured the first `captured` memory blocks of RAM, in
/// logical order, refuses the memory target with the expansion ranges `ranges`, of which the
/// one at `index` has `fault`, before it adds, onlines, or reseeds anything.
fn refuses(case: &str, captured: u64, ranges: &[(u64, u64)], index: usize, fault: RangeFault) {
    let calls = Calls::default();
    let system = FakeSystem::new(&calls).capturing(captured);
    let error = repairing(restore().with_memory_target(ranges), system)
        .run()
        .unwrap_err();
    assert_eq!(error.stage(), RepairStage::Memory, "{case}");
    assert!(
        matches!(
            error.cause(),
            RepairCause::Range { index: faulty, fault: found } if *faulty == index && *found == fault
        ),
        "{case}: {error}"
    );
    assert_eq!(
        calls.take(),
        [Call::OnlineCpus, Call::BlockSize, Call::MemoryBlocks],
        "{case}"
    );
}

#[test]
fn a_target_that_does_not_continue_the_captured_ram_is_refused_before_any_addition() {
    use RangeFault::{Aperture, Extra, Gap, Misaligned, Overlap};

    // The clone captured 512 MiB of RAM, 2.5 GiB of it, or all 3 GiB of low RAM.
    refuses("a gap", 4, &[(640 * MIB, 128 * MIB)], 0, Gap);
    refuses("an overlap", 4, &[(384 * MIB, 256 * MIB)], 0, Overlap);
    refuses("captured RAM", 4, &[(0, 128 * MIB)], 0, Overlap);
    let unaligned = [(512 * MIB + 4096, 128 * MIB)];
    refuses("a misaligned start", 4, &unaligned, 0, Misaligned);
    let partial = [(512 * MIB, 64 * MIB)];
    refuses("a misaligned length", 4, &partial, 0, Misaligned);
    refuses("an empty range", 4, &[(512 * MIB, 0)], 0, Misaligned);
    refuses("an overflow", 4, &[(512 * MIB, u64::MAX)], 0, Misaligned);
    refuses("past low RAM", 20, &[(2560 * MIB, GIB)], 0, Aperture);
    refuses("the aperture", 24, &[(3 * GIB, 128 * MIB)], 0, Aperture);
    let low = [(512 * MIB, 128 * MIB), (640 * MIB, 128 * MIB)];
    refuses("another range of low RAM", 4, &low, 1, Extra);
    let high = [(4 * GIB, 128 * MIB), (4224 * MIB, 128 * MIB)];
    refuses("another range of high RAM", 24, &high, 1, Extra);
    let reversed = [(4 * GIB, 128 * MIB), (2 * GIB, 128 * MIB)];
    refuses("low RAM after high RAM", 24, &reversed, 1, Extra);
    let early = [(512 * MIB, 128 * MIB), (4 * GIB, 128 * MIB)];
    refuses("high RAM before low RAM's end", 4, &early, 1, Gap);
}

#[test]
fn a_wrong_block_size_ranges_without_a_target_and_unordered_captured_ram_are_refused() {
    let calls = Calls::default();
    // A kernel with 1 GiB memory blocks.
    let mut system = FakeSystem::new(&calls);
    system.block_size = b"40000000\n";
    let target = restore().with_memory_target(&[(512 * MIB, 512 * MIB)]);
    let error = repairing(target, system).run().unwrap_err();
    assert_eq!(error.stage(), RepairStage::Memory);
    assert!(matches!(error.cause(), RepairCause::BlockSize(size) if size == "40000000"));
    assert_eq!(calls.take(), [Call::OnlineCpus, Call::BlockSize]);
    // Without a target, the online memory counts in blocks of any size.
    let mut system = FakeSystem::new(&calls);
    system.block_size = b"40000000\n";
    let repaired = repairing(restore(), system).run().unwrap();
    assert_eq!(repaired.memory(), 4 * GIB);
    calls.take();

    // Ranges that the restore lists without a memory target.
    let untargeted = restore().with_memory_ranges(&[(512 * MIB, 128 * MIB)]);
    let error = repairing(untargeted, FakeSystem::new(&calls))
        .run()
        .unwrap_err();
    assert_eq!(error.stage(), RepairStage::Memory);
    assert!(matches!(error.cause(), RepairCause::UntargetedRanges));
    assert_eq!(calls.take(), [Call::OnlineCpus]);

    // Captured RAM whose blocks are not the first ones of RAM.
    let mut system = FakeSystem::new(&calls);
    system.blocks.remove(&2);
    let target = restore().with_memory_target(&[(384 * MIB, 128 * MIB)]);
    let error = repairing(target, system).run().unwrap_err();
    assert_eq!(error.stage(), RepairStage::Memory);
    assert!(matches!(error.cause(), RepairCause::CapturedRam));
    assert_eq!(
        calls.take(),
        [Call::OnlineCpus, Call::BlockSize, Call::MemoryBlocks]
    );
}

#[test]
fn a_retry_after_a_failed_activation_adds_no_block_twice() {
    let calls = Calls::default();
    let mut system = FakeSystem::new(&calls);
    system.add_failures.insert(768 * MIB, Errno::EIO);
    system.online_failures.insert(7, Errno::EBUSY);
    let target = restore().with_memory_target(&[(512 * MIB, 512 * MIB)]);
    let mut repair = repairing(target, system);
    let error = repair.run().unwrap_err();
    assert_eq!(error.stage(), RepairStage::Memory);
    assert!(matches!(
        error.cause(),
        RepairCause::Io(error) if error.raw_os_error() == Some(Errno::EIO as i32)
    ));
    let mut expected = vec![Call::MemoryBlocks];
    expected.extend(activations([4, 5]));
    expected.push(Call::Add(768 * MIB));
    assert_eq!(changes(calls.take()), expected);

    // The retry keeps the captured RAM's end, so that the blocks that the first attempt added
    // count as done, not as captured RAM: it neither lists the blocks before it adds one, nor
    // adds one twice.
    let error = repair.run().unwrap_err();
    assert!(matches!(
        error.cause(),
        RepairCause::Io(error) if error.raw_os_error() == Some(Errno::EBUSY as i32)
    ));
    let mut expected = activations([6]);
    expected.extend([Call::Add(896 * MIB), Call::Online(7)]);
    assert_eq!(changes(calls.take()), expected);

    // The next retry onlines the block that failed to come online, without adding it again.
    let repaired = repair.run().unwrap();
    assert_eq!(repaired.memory(), GIB);
    let mut expected = vec![Call::Online(7)];
    expected.extend(finishing());
    assert_eq!(changes(calls.take()), expected);
}

#[test]
fn a_block_that_stays_offline_fails_each_activation() {
    let calls = Calls::default();
    let mut system = FakeSystem::new(&calls);
    system.stuck = Some(5);
    let target = restore().with_memory_target(&[(512 * MIB, 256 * MIB)]);
    let mut repair = repairing(target, system);
    for _ in 0..2 {
        let error = repair.run().unwrap_err();
        assert_eq!(error.stage(), RepairStage::Memory);
        assert!(matches!(
            error.cause(),
            RepairCause::Offline { block: 5, state } if state == "offline"
        ));
    }
    let mut expected = vec![Call::MemoryBlocks];
    expected.extend(activations([4, 5]));
    expected.push(Call::Online(5));
    assert_eq!(changes(calls.take()), expected);
}

#[test]
fn the_reseed_runs_once_and_its_failure_is_final() {
    let calls = Calls::default();
    let mut system = FakeSystem::new(&calls);
    system.reseed_failure = Some(Errno::EPERM);
    let mut repair = repairing(restore(), system);
    let error = repair.run().unwrap_err();
    assert_eq!(error.stage(), RepairStage::Reseed);
    assert!(matches!(
        error.cause(),
        RepairCause::Reseed(ReseedError::Reseed(_))
    ));
    assert_eq!(
        calls.take(),
        [
            Call::OnlineCpus,
            Call::BlockSize,
            Call::MemoryBlocks,
            Call::Reseed(RESTORED)
        ]
    );
    // The failed reseed dropped the seed all the same, which zeroed it.
    assert_eq!(take_dropped(), [[0; Seed::LENGTH]]);
    // Every retry returns the same error, without a second reseed or a finish.
    for _ in 0..2 {
        let again = repair.run().unwrap_err();
        assert!(Arc::ptr_eq(&again.cause, &error.cause));
    }
    assert_eq!(calls.take(), Vec::<Call>::new());
}

#[test]
fn the_finish_runs_once_and_its_failure_is_final() {
    let calls = Calls::default();
    let mut system = FakeSystem::new(&calls);
    system.finish_fails = true;
    let mut repair = repairing(restore(), system);
    let error = repair.run().unwrap_err();
    assert_eq!(error.stage(), RepairStage::Finish);
    assert!(matches!(
        error.cause(),
        RepairCause::Time(TimeError::StateUnavailable)
    ));
    let mut expected = vec![Call::OnlineCpus, Call::BlockSize];
    expected.extend(finishing());
    assert_eq!(calls.take(), expected);
    // Every retry returns the same error, without a second acknowledgement.
    for _ in 0..2 {
        let again = repair.run().unwrap_err();
        assert!(Arc::ptr_eq(&again.cause, &error.cause));
    }
    assert_eq!(calls.take(), Vec::<Call>::new());
}

#[test]
fn a_finish_that_never_returned_never_runs_again() {
    let calls = Calls::default();
    let mut system = FakeSystem::new(&calls);
    system.finish_panics = true;
    let mut repair = repairing(restore(), system);
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| repair.run()));
    assert!(unwound.is_err());
    assert_eq!(
        calls.take().last(),
        Some(&Call::Finish(RestoreFinish::new().acknowledge()))
    );
    for _ in 0..2 {
        let error = repair.run().unwrap_err();
        assert_eq!(error.stage(), RepairStage::Finish);
        assert!(matches!(error.cause(), RepairCause::Incomplete));
    }
    assert_eq!(calls.take(), Vec::<Call>::new());
}

/// Returns the error with which the repair of `restore` on `system` fails.
fn failure(restore: Restore, system: FakeSystem) -> RepairError {
    repairing(restore, system).run().unwrap_err()
}

#[test]
fn each_failure_class_ends_the_clone_with_a_status_of_its_own() {
    let calls = Calls::default();
    let system = || FakeSystem::new(&calls);
    let target = || restore().with_memory_target(&[(512 * MIB, 256 * MIB)]);
    let mut cases = Vec::new();
    // The restore.
    let unseeded = Restore::to(RESTORED).gated();
    cases.push((
        "no seed",
        failure(unseeded, system()),
        status::REPAIR_RESTORE,
    ));
    let finished = restore().finished_by_capture();
    cases.push((
        "finished",
        failure(finished, system()),
        status::REPAIR_RESTORE,
    ));
    // The processors.
    let mut unlisted = system();
    unlisted.cpus = b"1-3\n".to_vec();
    let error = failure(restore(), unlisted);
    cases.push(("online CPUs", error, status::REPAIR_PROCESSORS));
    let error = failure(restore().with_processor_target(1), system());
    cases.push(("a processor target", error, status::REPAIR_PROCESSORS));
    let mut stuck_cpu = system();
    stuck_cpu.stuck_cpu = Some(3);
    let error = failure(restore().with_processor_target(4), stuck_cpu);
    cases.push(("an offline CPU", error, status::REPAIR_PROCESSORS));
    // The memory target, which the repair refuses before it adds any memory block.
    let mut wide = system();
    wide.block_size = b"40000000\n";
    let error = failure(target(), wide);
    cases.push(("the block size", error, status::REPAIR_MEMORY_TARGET));
    let untargeted = restore().with_memory_ranges(&[(512 * MIB, 128 * MIB)]);
    let error = failure(untargeted, system());
    cases.push(("untargeted ranges", error, status::REPAIR_MEMORY_TARGET));
    let mut unordered = system();
    unordered.blocks.remove(&2);
    let error = failure(target(), unordered);
    cases.push(("the captured RAM", error, status::REPAIR_MEMORY_TARGET));
    let overlap = restore().with_memory_target(&[(384 * MIB, 256 * MIB)]);
    let error = failure(overlap, system());
    cases.push(("a range", error, status::REPAIR_MEMORY_TARGET));
    // The memory target's activation.
    let mut refusing = system();
    refusing.add_failures.insert(512 * MIB, Errno::EIO);
    let error = failure(target(), refusing);
    cases.push(("an addition", error, status::REPAIR_MEMORY));
    let mut stuck = system();
    stuck.stuck = Some(5);
    let error = failure(target(), stuck);
    cases.push(("an offline block", error, status::REPAIR_MEMORY));
    // The reseed.
    let mut refused = system();
    refused.reseed_failure = Some(Errno::EPERM);
    let error = failure(restore(), refused);
    cases.push(("the reseed", error, status::REPAIR_RESEED));
    // The finish, and one that never returned.
    let mut unfinished = system();
    unfinished.finish_fails = true;
    let error = failure(restore(), unfinished);
    cases.push(("the finish", error, status::REPAIR_FINISH));
    let mut panicking = system();
    panicking.finish_panics = true;
    let mut repair = repairing(restore(), panicking);
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| repair.run()));
    assert!(unwound.is_err());
    let error = repair.run().unwrap_err();
    cases.push(("an incomplete finish", error, status::REPAIR_FINISH));
    // The violations of the time ABI, whose statuses they keep.
    let mut unrepaired = system();
    unrepaired.finish_violation = Some(Violation::of_repair("the time state is unreadable"));
    let error = failure(restore(), unrepaired);
    cases.push(("the time ABI's repair", error, status::TIME_REPAIR));
    let mut undaemoned = system();
    undaemoned.finish_violation = Some(Violation::of_missing_daemon());
    let error = failure(restore(), undaemoned);
    cases.push(("the time daemon", error, status::TIME_CONFORMANCE));

    let mut classes = Vec::new();
    for (case, error, class) in cases {
        assert_eq!(error.status(), class, "{case}: {error}");
        classes.push(class);
    }
    // The cases cover every status of the repair, and the time ABI's statuses of a strict call.
    classes.sort_unstable();
    classes.dedup();
    let mut expected: Vec<_> = status::REPAIR_STATUSES.collect();
    expected.extend([status::TIME_CONFORMANCE, status::TIME_REPAIR]);
    assert_eq!(classes, expected);
}

#[test]
fn a_violation_of_the_strict_finish_is_final() {
    let calls = Calls::default();
    let mut system = FakeSystem::new(&calls);
    system.finish_violation = Some(Violation::of_missing_daemon());
    let mut repair = repairing(restore(), system);
    let error = repair.run().unwrap_err();
    assert_eq!(error.stage(), RepairStage::Finish);
    assert_eq!(
        error.to_string(),
        "failed to finish the restore: G_CONFORMANCE_C10: the time daemon is not running"
    );
    let mut expected = vec![Call::OnlineCpus, Call::BlockSize];
    expected.extend(finishing());
    assert_eq!(calls.take(), expected);
    // Every retry returns the same error, with its status, without a second acknowledgement.
    for _ in 0..2 {
        let again = repair.run().unwrap_err();
        assert!(Arc::ptr_eq(&again.cause, &error.cause));
        assert_eq!(again.status(), status::TIME_CONFORMANCE);
    }
    assert_eq!(calls.take(), Vec::<Call>::new());
}

#[test]
fn errors_report_how_far_the_repair_got() {
    let calls = Calls::default();
    let mut system = FakeSystem::new(&calls);
    system.add_failures.insert(768 * MIB, Errno::EIO);
    system.online_failures.insert(7, Errno::EBUSY);
    let target = restore().with_memory_target(&[(512 * MIB, 512 * MIB)]);
    let mut repair = repairing(target, system);
    // The first attempt added and onlined two blocks before the third's addition failed.
    let progress = repair.run().unwrap_err().progress();
    assert_eq!((progress.added(), progress.onlined()), (2, 2), "{progress}");
    assert!(!progress.reseeded() && !progress.finish_started());
    assert_eq!(progress.to_string(), "added 2 memory blocks, onlined 2");
    // The retry added the third and the fourth, and onlined the third, before the fourth's
    // onlining failed: the progress counts the attempts' work together.
    let progress = repair.run().unwrap_err().progress();
    assert_eq!((progress.added(), progress.onlined()), (4, 3), "{progress}");
    repair.run().unwrap();

    // A failed finish follows the reseed, and may have acknowledged the restore. The kernel
    // brought the blocks that the repair added online itself.
    let mut system = FakeSystem::new(&calls);
    system.auto_online = true;
    system.finish_fails = true;
    let target = restore().with_memory_target(&[(512 * MIB, 256 * MIB)]);
    let mut repair = repairing(target, system);
    let progress = repair.run().unwrap_err().progress();
    assert_eq!((progress.added(), progress.onlined()), (2, 0), "{progress}");
    assert!(progress.reseeded() && progress.finish_started());
    assert_eq!(
        progress.to_string(),
        "added 2 memory blocks, onlined 0, reseeded, started the finish"
    );
    // A final failure keeps its progress.
    assert_eq!(repair.run().unwrap_err().progress(), progress);

    // A failed reseed reseeded nothing, and started no finish.
    let mut system = FakeSystem::new(&calls);
    system.reseed_failure = Some(Errno::EPERM);
    let error = repairing(restore(), system).run().unwrap_err();
    assert_eq!(error.progress(), RepairProgress::default());
    assert_eq!(
        error.progress().to_string(),
        "added 0 memory blocks, onlined 0"
    );
}

#[test]
fn a_block_counts_once_the_repair_finds_it_added_or_online() {
    let calls = Calls::default();
    let mut system = FakeSystem::new(&calls);
    // The reads of a block's state come before its addition, then before its onlining, then
    // after it: block 4's state cannot be read once it was added, nor block 5's once it was
    // onlined.
    system.read_failures.insert((4, 1), Errno::EIO);
    system.read_failures.insert((5, 2), Errno::EIO);
    system.reseed_failure = Some(Errno::EPERM);
    let target = restore().with_memory_target(&[(512 * MIB, 256 * MIB)]);
    let mut repair = repairing(target, system);
    let expected = |added, onlined| RepairProgress {
        added,
        onlined,
        ..RepairProgress::default()
    };
    // The kernel added block 4, which the attempt could not find.
    let error = repair.run().unwrap_err();
    assert!(matches!(
        error.cause(),
        RepairCause::Io(error) if error.raw_os_error() == Some(Errno::EIO as i32)
    ));
    assert_eq!(error.progress(), expected(0, 0));
    assert_eq!(
        changes(calls.take()),
        [Call::MemoryBlocks, Call::Add(512 * MIB)]
    );
    // The retry finds block 4 added, and onlines it, then adds and onlines block 5, which it
    // could not find online.
    let error = repair.run().unwrap_err();
    assert_eq!(error.stage(), RepairStage::Memory);
    assert_eq!(error.progress(), expected(2, 1));
    let mut activated = vec![Call::Online(4)];
    activated.extend(activations([5]));
    assert_eq!(changes(calls.take()), activated);
    // The next finds block 5 online, which counts once, and adds and onlines nothing.
    let error = repair.run().unwrap_err();
    assert_eq!(error.stage(), RepairStage::Reseed);
    assert_eq!(error.progress(), expected(2, 2));
    assert_eq!(
        changes(calls.take()),
        [Call::MemoryBlocks, Call::Reseed(RESTORED)]
    );
    // A final failure keeps the count.
    assert_eq!(repair.run().unwrap_err().progress(), expected(2, 2));
}

/// Checks that the repair of a clone whose kernel lists the online CPUs `cpus`, and whose restore
/// has the processor target `target`, if any, counts `online` CPUs, and finishes the restore
/// with `new_cpus`, the CPUs that it brought online.
fn counts(cpus: &'static [u8], target: Option<u8>, online: u32, new_cpus: CpuSet) {
    let calls = Calls::default();
    let mut system = FakeSystem::new(&calls);
    system.cpus = cpus.to_vec();
    let restore = match target {
        Some(target) => restore().with_processor_target(target),
        None => restore(),
    };
    let repaired = repairing(restore, system).run().unwrap();
    assert_eq!(repaired.cpus(), online, "{cpus:?}");
    let finish = RestoreFinish::new().acknowledge().new_cpus(new_cpus);
    assert_eq!(calls.take().last(), Some(&Call::Finish(finish)), "{cpus:?}");
}

/// Checks that the repair of a clone whose kernel lists the online CPUs `cpus`, and whose restore
/// has the processor target `target`, if any, fails with `cause`, before any other call.
fn refuses_cpus(cpus: &'static [u8], target: Option<u8>, cause: &RepairCause) {
    let calls = Calls::default();
    let mut system = FakeSystem::new(&calls);
    system.cpus = cpus.to_vec();
    let restore = match target {
        Some(target) => restore().with_processor_target(target),
        None => restore(),
    };
    let error = repairing(restore, system).run().unwrap_err();
    assert_eq!(error.stage(), RepairStage::Processors, "{cpus:?}");
    assert_eq!(error.cause().to_string(), cause.to_string());
    assert_eq!(calls.take(), [Call::OnlineCpus], "{cpus:?}");
}

#[test]
fn the_online_cpus_must_be_a_prefix_that_the_processor_target_does_not_undercut() {
    counts(b"0\n", None, 1, CpuSet::new());
    counts(b"0-3\n", None, 4, CpuSet::new());
    counts(b"0-3\n", Some(4), 4, CpuSet::new());
    // The repair takes no CPU offline.
    let cause = |target, online| RepairCause::ProcessorTarget { target, online };
    refuses_cpus(b"0-3\n", Some(2), &cause(2, 4));
    refuses_cpus(b"0-7\n", Some(1), &cause(1, 8));
    let lists: [&'static [u8]; 4] = [b"0-1,3\n", b"1-3\n", b"0-\n", b"\n"];
    for cpus in lists {
        let listed = String::from_utf8_lossy(cpus.strip_suffix(b"\n").unwrap()).into_owned();
        refuses_cpus(cpus, None, &RepairCause::OnlineCpus(listed.clone()));
        refuses_cpus(cpus, Some(8), &RepairCause::OnlineCpus(listed));
    }
}

#[test]
fn a_processor_target_brings_the_offline_cpus_of_its_prefix_online_and_finishes_with_them() {
    counts(b"0\n", Some(2), 2, cpu_set([1]));
    counts(b"0-1\n", Some(8), 8, cpu_set(2..8));
    // The repair brings each CPU online in order, before the memory target, and verifies them.
    let calls = Calls::default();
    let mut system = FakeSystem::new(&calls);
    system.cpus = b"0\n".to_vec();
    let target = restore().with_processor_target(4);
    let repaired = repairing(target, system).run().unwrap();
    assert_eq!(repaired.cpus(), 4);
    let mut expected = vec![
        Call::OnlineCpus,
        Call::OnlineCpu(1),
        Call::OnlineCpu(2),
        Call::OnlineCpu(3),
        Call::OnlineCpus,
        Call::BlockSize,
    ];
    let mut finished = finishing();
    finished[3] = Call::Finish(RestoreFinish::new().acknowledge().new_cpus(cpu_set(1..4)));
    expected.extend(finished);
    assert_eq!(calls.take(), expected);
}

#[test]
fn a_retry_brings_online_only_the_cpus_that_are_not_and_reports_every_new_one() {
    let calls = Calls::default();
    let mut system = FakeSystem::new(&calls);
    system.cpus = b"0\n".to_vec();
    system.cpu_failures.insert(2, Errno::EBUSY);
    let mut repair = repairing(restore().with_processor_target(4), system);
    // The first attempt brought CPU 1 online before CPU 2 failed.
    let error = repair.run().unwrap_err();
    assert_eq!(error.stage(), RepairStage::Processors);
    assert!(matches!(
        error.cause(),
        RepairCause::Io(error) if error.raw_os_error() == Some(Errno::EBUSY as i32)
    ));
    assert_eq!(error.status(), status::REPAIR_PROCESSORS);
    assert_eq!(
        changes(calls.take()),
        [Call::OnlineCpu(1), Call::OnlineCpu(2)]
    );
    // The retry brings CPUs 2 and 3 online, and the finish names the three that the repair
    // brought online.
    assert_eq!(repair.run().unwrap().cpus(), 4);
    let calls = changes(calls.take());
    assert_eq!(calls[..2], [Call::OnlineCpu(2), Call::OnlineCpu(3)]);
    let finish = RestoreFinish::new().acknowledge().new_cpus(cpu_set(1..4));
    assert_eq!(calls.last(), Some(&Call::Finish(finish)));
}

#[test]
fn a_cpu_that_stays_offline_fails_the_activation_of_the_processor_target() {
    let calls = Calls::default();
    let mut system = FakeSystem::new(&calls);
    system.stuck_cpu = Some(2);
    let error = failure(restore().with_processor_target(4), system);
    assert_eq!(error.stage(), RepairStage::Processors);
    assert_eq!(
        error.to_string(),
        "failed to activate the processor target: the online CPUs are 0-1 once the first 4 \
         were brought online"
    );
    // The repair went no further.
    assert_eq!(
        calls.take(),
        [
            Call::OnlineCpus,
            Call::OnlineCpu(2),
            Call::OnlineCpu(3),
            Call::OnlineCpus
        ]
    );
}

#[test]
fn a_memory_target_that_fails_after_the_processor_target_leaves_its_cpus_online() {
    // A refused memory target adds no memory block, but the processor target before it is
    // active already, and stays so.
    let calls = Calls::default();
    let mut system = FakeSystem::new(&calls);
    system.cpus = b"0\n".to_vec();
    system.block_size = b"40000000\n";
    let targets = restore()
        .with_processor_target(2)
        .with_memory_target(&[(512 * MIB, 512 * MIB)]);
    let error = repairing(targets, system).run().unwrap_err();
    assert_eq!(error.stage(), RepairStage::Memory);
    assert_eq!(error.status(), status::REPAIR_MEMORY_TARGET);
    assert_eq!(
        calls.take(),
        [
            Call::OnlineCpus,
            Call::OnlineCpu(1),
            Call::OnlineCpus,
            Call::BlockSize
        ]
    );

    // A retry after a failed activation of the memory target brings no CPU online again, and the
    // finish names every CPU that the repair brought online.
    let mut system = FakeSystem::new(&calls);
    system.cpus = b"0\n".to_vec();
    system.add_failures.insert(512 * MIB, Errno::EIO);
    let targets = restore()
        .with_processor_target(4)
        .with_memory_target(&[(512 * MIB, 128 * MIB)]);
    let mut repair = repairing(targets, system);
    let error = repair.run().unwrap_err();
    assert_eq!(error.status(), status::REPAIR_MEMORY);
    assert_eq!(
        changes(calls.take()),
        [
            Call::OnlineCpu(1),
            Call::OnlineCpu(2),
            Call::OnlineCpu(3),
            Call::MemoryBlocks,
            Call::Add(512 * MIB)
        ]
    );
    let repaired = repair.run().unwrap();
    assert_eq!((repaired.cpus(), repaired.memory()), (4, 640 * MIB));
    let mut expected = activations([4]);
    let mut finished = finishing();
    finished[3] = Call::Finish(RestoreFinish::new().acknowledge().new_cpus(cpu_set(1..4)));
    expected.extend(finished);
    assert_eq!(changes(calls.take()), expected);
}

#[test]
fn a_restore_without_its_seed_or_that_its_capture_finished_is_refused() {
    let calls = Calls::default();
    let error = repairing(Restore::to(RESTORED).gated(), FakeSystem::new(&calls))
        .run()
        .unwrap_err();
    assert_eq!(error.stage(), RepairStage::Restore);
    assert!(matches!(error.cause(), RepairCause::NoSeed));
    let finished = restore().finished_by_capture();
    let error = repairing(finished, FakeSystem::new(&calls))
        .run()
        .unwrap_err();
    assert_eq!(error.stage(), RepairStage::Restore);
    assert!(matches!(error.cause(), RepairCause::Finished));
    assert_eq!(calls.take(), Vec::<Call>::new());
}

#[test]
fn errors_name_their_stage_and_their_cause() {
    use std::error::Error;

    let memory = |cause| RepairError::new(RepairStage::Memory, cause);
    let cases = [
        (
            memory(RepairCause::Io(at(
                Path::new("/sys/x"),
                &io::Error::from(Errno::EIO),
            ))),
            "failed to activate the memory target: /sys/x: I/O error",
        ),
        (
            memory(RepairCause::Range {
                index: 0,
                fault: RangeFault::Overlap,
            }),
            "failed to activate the memory target: memory range 0 overlaps the RAM before it",
        ),
        (
            memory(RepairCause::Range {
                index: 1,
                fault: RangeFault::Aperture,
            }),
            "failed to activate the memory target: memory range 1 enters the MMIO aperture \
             between 3 GiB and 4 GiB",
        ),
        (
            memory(RepairCause::BlockSize("40000000".to_owned())),
            "failed to activate the memory target: the memory block size is 40000000 in \
             hexadecimal, not 128 MiB",
        ),
        (
            memory(RepairCause::Offline {
                block: 5,
                state: "offline".to_owned(),
            }),
            "failed to activate the memory target: memory block 5 is offline, not online",
        ),
        (
            RepairError::new(
                RepairStage::Processors,
                RepairCause::ProcessorTarget {
                    target: 2,
                    online: 4,
                },
            ),
            "failed to activate the processor target: the restore asks for 2 online CPUs, and 4 \
             are online",
        ),
        (
            RepairError::new(RepairStage::Restore, RepairCause::NoSeed),
            "failed to check the restore: the restore holds no seed",
        ),
        (
            RepairError::new(
                RepairStage::Reseed,
                RepairCause::Reseed(ReseedError::Reseed(Errno::EPERM.into())),
            ),
            "failed to reseed the kernel's random number generator: random ioctl: Operation not \
             permitted",
        ),
        (
            RepairError::new(
                RepairStage::Finish,
                RepairCause::Time(TimeError::StateUnavailable),
            ),
            "failed to finish the restore: the time state is unavailable",
        ),
        (
            RepairError::new(RepairStage::Finish, RepairCause::Incomplete),
            "failed to finish the restore: an earlier attempt never returned",
        ),
    ];
    for (error, text) in cases {
        assert_eq!(error.to_string(), text);
    }
    assert!(
        memory(RepairCause::Io(Errno::EIO.into()))
            .source()
            .is_some()
    );
    let reseed = RepairError::new(
        RepairStage::Reseed,
        RepairCause::Reseed(ReseedError::Reseed(Errno::EPERM.into())),
    );
    assert!(reseed.source().is_some());
    assert!(memory(RepairCause::CapturedRam).source().is_none());
}

#[test]
fn a_repair_draws_no_randomness_before_its_reseed() {
    // A seccomp filter lasts as long as the thread that installs it, and passes to the processes
    // that the thread starts, so the test runs in a process of its own.
    if crate::guest::managed_agent::isolate_process_test(
        "NVX_CLONE_REPAIR_RANDOMNESS_TEST_CHILD",
        "guest::clone::repair::tests::a_repair_draws_no_randomness_before_its_reseed",
    ) {
        return;
    }
    let calls = Calls::default();
    let target = restore().with_memory_target(&[(512 * MIB, 512 * MIB)]);
    let mut repair = repairing(target, FakeSystem::new(&calls));
    // From here on, `getrandom`, or an open, which a read of `/dev/urandom` or `/dev/random`
    // needs, ends the process with `SIGSYS`. The fake system opens no file, so only randomness
    // that the repair drew, such as the keys of a hashed map, would make either call.
    sys::forbid_system_calls(&[
        nix::libc::SYS_getrandom,
        nix::libc::SYS_open,
        nix::libc::SYS_openat,
        nix::libc::SYS_openat2,
    ]);
    let repaired = repair.run().unwrap();
    assert_eq!(repaired.memory(), GIB);
    let mut expected = vec![Call::OnlineCpus, Call::BlockSize, Call::MemoryBlocks];
    expected.extend(activations(4..8));
    expected.extend(finishing());
    assert_eq!(calls.take(), expected);
}

/// Writes `contents` to the file at `path`, below directories that it creates.
fn write_file(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

#[test]
fn the_live_system_reads_and_writes_the_kernel_s_cpu_and_memory_interfaces() {
    let root = tempfile::tempdir().unwrap();
    let memory = root.path().join(MEMORY_BLOCKS);
    let cpus = root.path().join(CPUS);
    write_file(&root.path().join(ONLINE_CPUS), "0-3\n");
    write_file(&cpus.join("cpu4/online"), "");
    write_file(&memory.join(BLOCK_SIZE_FILE), "8000000\n");
    write_file(&memory.join(PROBE_FILE), "");
    write_file(&memory.join("memory0/state"), "online\n");
    write_file(&memory.join("memory12/state"), "offline\n");
    write_file(&memory.join("memory2/state"), "online\n");
    // None of these is a memory block.
    write_file(&memory.join("auto_online_blocks"), "offline\n");
    fs::create_dir(memory.join("power")).unwrap();
    fs::create_dir(memory.join("memoryx")).unwrap();
    let mut system = LiveSystem {
        paths: Paths {
            proc: root.path().join("proc"),
            sys: root.path().to_owned(),
            dev: root.path().join("dev"),
        },
        abi: TimeAbi::report_only(),
    };
    assert_eq!(system.online_cpus().unwrap(), b"0-3\n");
    // Bringing a CPU online writes its new state, as `nvx-snapshot` does; a CPU without a state
    // file cannot be brought online, and the error names the file.
    system.online_cpu(4).unwrap();
    assert_eq!(fs::read(cpus.join("cpu4/online")).unwrap(), b"1\n");
    let error = system.online_cpu(5).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    assert!(!cpus.join("cpu5/online").exists());
    let named = format!("{}: ", cpus.join("cpu5/online").display());
    assert!(error.to_string().starts_with(&named), "{error}");
    assert_eq!(system.block_size().unwrap(), b"8000000\n");
    assert_eq!(system.memory_blocks().unwrap(), [0, 2, 12]);
    assert_eq!(system.block_state(12).unwrap().unwrap(), b"offline\n");
    assert_eq!(system.block_state(4).unwrap(), None);
    // An addition writes the block's physical address to the probe file, and an onlining writes
    // the block's new state, as `nvx-snapshot` does. sysfs takes each write whole, so the test
    // empties the state file before the onlining writes it.
    system.add_block(4 * BLOCK_SIZE).unwrap();
    assert_eq!(fs::read(memory.join(PROBE_FILE)).unwrap(), b"0x20000000\n");
    fs::write(memory.join("memory12/state"), "").unwrap();
    system.online_block(12).unwrap();
    assert_eq!(
        fs::read(memory.join("memory12/state")).unwrap(),
        b"online\n"
    );
    // A write creates no file, and errors name the file.
    let state = memory.join("memory4/state");
    let error = system.online_block(4).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    assert!(!state.exists());
    let named = format!("{}: ", state.display());
    assert!(error.to_string().starts_with(&named), "{error}");
    fs::remove_file(memory.join(BLOCK_SIZE_FILE)).unwrap();
    let error = system.block_size().unwrap_err();
    let named = format!("{}: ", memory.join(BLOCK_SIZE_FILE).display());
    assert!(error.to_string().starts_with(&named), "{error}");
}
