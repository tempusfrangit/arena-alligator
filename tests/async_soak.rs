//! Seeded cancellation soak for the async allocation paths of both arenas.
//!
//! `ARENA_SOAK_TASKS` tasks on a multi-threaded runtime make async
//! allocations, keep each buffer for a short time and free it. Sync
//! allocations that take freed memory occur at the same time. The test
//! cancels async allocations at random points. It cancels before the first
//! poll, after a wake but before the woken future runs, and by a deadline. A
//! deadline can occur while the future is parked or while the task has the
//! buffer.
//!
//! During the run, the test makes sure that two tasks never own the same slot
//! or block at the same time. A side table of owned address ranges finds
//! overlaps. Each task also writes an owner tag over the whole buffer and reads
//! it back from the frozen bytes before release. The metrics must never be
//! behind what the tasks observed. No waiter can stay parked with enough free
//! memory for it while no other allocation makes progress. At the end, all
//! tasks must finish, the metrics must agree with the observed allocations,
//! and the arena must allocate fully again.
//!
//! The default run is small enough for the pre-push hook. Environment
//! variables select a larger run. An unset variable keeps its default value:
//!
//! - `ARENA_SOAK_SEED`: replay one seed. A failure prints the seed.
//! - `ARENA_SOAK_SEEDS`: number of seeds, run as 1..=N (default 3).
//! - `ARENA_SOAK_TASKS`: concurrent tasks per round (default 16).
//! - `ARENA_SOAK_OPS`: operations per task (default 4).
//! - `ARENA_SOAK_ROUNDS`: rounds per seed. Each round starts its tasks
//!   together and waits until all of them finish (default 12).
//! - `ARENA_SOAK_SECS`: after the minimum rounds, start new rounds until each
//!   seed runs for this number of seconds (default 0).
//! - `ARENA_SOAK_UNITS`: arena capacity in minimum blocks, a power of two
//!   (default 8).
//! - `ARENA_SOAK_THREADS`: runtime worker threads (default 4).
//! - `ARENA_SOAK_STALL_MS`: the maximum time that a waiter can stay parked
//!   next to enough free memory while no allocation succeeds (default 1000).
//! - `ARENA_SOAK_VERBOSE`: print per-seed counts.
//!
//! Large run, for example:
//!
//! ```text
//! ARENA_SOAK_SEEDS=20 ARENA_SOAK_TASKS=4000 ARENA_SOAK_OPS=20 ARENA_SOAK_ROUNDS=10 \
//!     ARENA_SOAK_UNITS=256 ARENA_SOAK_THREADS=8 ARENA_SOAK_VERBOSE=1 \
//!     cargo nextest run --release --all-features --test async_soak --no-capture
//! ```
#![cfg(feature = "async-alloc")]

use std::collections::BTreeMap;
use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

use arena_alligator::{
    AsyncBuddyArena, AsyncFixedArena, BuddyArena, BuddyGeometry, Buffer, FixedArena,
};
use bytes::{BufMut, Bytes};

/// Minimum block size of the buddy arena and slot size of the fixed arena.
const UNIT: usize = 64;

/// Largest buddy request order, so requests stay small next to the arena.
const MAX_REQUEST_ORDER: usize = 3;

const LIVENESS_DEADLINE: Duration = Duration::from_secs(60);

fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

fn env_u64(name: &str, default: u64) -> u64 {
    match std::env::var(name) {
        Ok(value) => value
            .parse()
            .unwrap_or_else(|_| panic!("{name}={value} is not a number")),
        Err(_) => default,
    }
}

#[derive(Clone, Copy, Debug)]
struct Config {
    tasks: usize,
    ops: usize,
    rounds: u64,
    units: usize,
    threads: usize,
    secs: u64,
    stall: Duration,
}

impl Config {
    fn from_env() -> Self {
        let units = env_u64("ARENA_SOAK_UNITS", 8) as usize;
        assert!(
            units.is_power_of_two(),
            "ARENA_SOAK_UNITS must be a power of two"
        );
        Self {
            tasks: env_u64("ARENA_SOAK_TASKS", 16) as usize,
            ops: env_u64("ARENA_SOAK_OPS", 4) as usize,
            rounds: env_u64("ARENA_SOAK_ROUNDS", 12),
            units,
            threads: env_u64("ARENA_SOAK_THREADS", 4) as usize,
            secs: env_u64("ARENA_SOAK_SECS", 0),
            stall: Duration::from_millis(env_u64("ARENA_SOAK_STALL_MS", 1000)),
        }
    }

    fn seeds() -> Vec<u64> {
        match std::env::var("ARENA_SOAK_SEED") {
            Ok(seed) => vec![seed.parse().expect("ARENA_SOAK_SEED is not a number")],
            Err(_) => (1..=env_u64("ARENA_SOAK_SEEDS", 3)).collect(),
        }
    }
}

/// SplitMix64.
#[derive(Clone)]
struct Rng(u64);

impl Rng {
    fn new(seed: u64, stream: u64) -> Self {
        let mut rng = Self(seed ^ stream.wrapping_mul(0xA076_1D64_78BD_642F));
        rng.next();
        rng
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

/// What the tasks observed, plus the checks that compare it with the arena.
#[derive(Default)]
struct Ledger {
    /// Owned address ranges: start -> (end, owner tag).
    owners: Mutex<BTreeMap<usize, (usize, u64)>>,
    violations: Mutex<Vec<String>>,
    next_tag: AtomicU64,
    acquired: AtomicU64,
    released: AtomicU64,
    frozen: AtomicU64,
    sync_failed: AtomicU64,
    cancelled: AtomicU64,
    /// Number of async allocation futures that are parked now, per request order.
    parked: [AtomicUsize; MAX_REQUEST_ORDER + 1],
    /// Longest time any allocation spent parked, per request order.
    longest_wait_us: [AtomicU64; MAX_REQUEST_ORDER + 1],
}

impl Ledger {
    fn violation(&self, message: String) {
        self.violations.lock().unwrap().push(message);
    }

    fn any_parked(&self) -> bool {
        self.parked.iter().any(|p| p.load(Ordering::Acquire) > 0)
    }

    fn smallest_parked_order(&self) -> Option<usize> {
        self.parked
            .iter()
            .position(|p| p.load(Ordering::Acquire) > 0)
    }
}

/// A buffer that the test owns, registered in the side table under a unique
/// tag. The drop examines the tag of a frozen buffer, removes the range from
/// the table, and then frees the memory. Because the free occurs last, the
/// range is out of the table before the arena can give it to a different owner.
struct Owned {
    ledger: Arc<Ledger>,
    start: usize,
    end: usize,
    tag: u64,
    buf: Option<Buffer>,
    bytes: Option<Bytes>,
}

impl Owned {
    fn new(ledger: &Arc<Ledger>, mut buf: Buffer, freeze: bool) -> Self {
        ledger.acquired.fetch_add(1, Ordering::Release);
        let tag = ledger.next_tag.fetch_add(1, Ordering::Relaxed) + 1;
        let start = buf.chunk_mut().as_mut_ptr() as usize;
        let end = start + buf.capacity();
        {
            let mut owners = ledger.owners.lock().unwrap();
            if let Some((&prev, &(prev_end, prev_tag))) = owners.range(..end).next_back()
                && prev_end > start
            {
                ledger.violation(format!(
                    "range {start:#x}..{end:#x} handed out while {prev:#x}..{prev_end:#x} \
                     is still owned by tag {prev_tag}"
                ));
            }
            owners.insert(start, (end, tag));
        }
        for _ in 0..buf.capacity() / 8 {
            buf.put_u64(tag);
        }
        let (buf, bytes) = if freeze {
            ledger.frozen.fetch_add(1, Ordering::Relaxed);
            (None, Some(buf.freeze()))
        } else {
            (Some(buf), None)
        };
        Self {
            ledger: Arc::clone(ledger),
            start,
            end,
            tag,
            buf,
            bytes,
        }
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        if let Some(bytes) = &self.bytes
            && let Some(other) = bytes
                .as_chunks::<8>()
                .0
                .iter()
                .map(|c| u64::from_be_bytes(*c))
                .find(|&t| t != self.tag)
        {
            self.ledger.violation(format!(
                "tag {} found tag {other} written into {:#x}..{:#x}",
                self.tag, self.start, self.end
            ));
        }
        let removed = self.ledger.owners.lock().unwrap().remove(&self.start);
        if removed != Some((self.end, self.tag)) {
            self.ledger.violation(format!(
                "tag {} releasing {:#x}..{:#x} found {removed:?} in the table",
                self.tag, self.start, self.end
            ));
        }
        drop(self.bytes.take());
        drop(self.buf.take());
        // The count occurs after the free, so a sampled `frees` is never behind it.
        self.ledger.released.fetch_add(1, Ordering::Release);
    }
}

/// Records wakes of the wrapped future and forwards them to the task.
#[derive(Default)]
struct WakeFlag {
    woken: AtomicBool,
    task: Mutex<Option<Waker>>,
}

impl Wake for WakeFlag {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.woken.store(true, Ordering::Release);
        if let Some(waker) = self.task.lock().unwrap().as_ref() {
            waker.wake_by_ref();
        }
    }
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Wraps an `allocate_async` future and keeps the per-order parked count. In
/// `cancel_after_wake` mode, it drops the inner future after a wake and before
/// the next poll.
struct Watched<'a> {
    inner: Option<BoxFuture<'a, Buffer>>,
    ledger: &'a Ledger,
    order: usize,
    cancel_after_wake: bool,
    flag: Arc<WakeFlag>,
    waker: Waker,
    parked_at: Option<Instant>,
}

impl<'a> Watched<'a> {
    fn new(
        inner: BoxFuture<'a, Buffer>,
        ledger: &'a Ledger,
        order: usize,
        cancel_after_wake: bool,
    ) -> Self {
        let flag = Arc::new(WakeFlag::default());
        let waker = Waker::from(Arc::clone(&flag));
        Self {
            inner: Some(inner),
            ledger,
            order,
            cancel_after_wake,
            flag,
            waker,
            parked_at: None,
        }
    }

    fn unpark(&mut self) {
        if let Some(since) = self.parked_at.take() {
            self.ledger.parked[self.order].fetch_sub(1, Ordering::AcqRel);
            let waited = since.elapsed().as_micros() as u64;
            self.ledger.longest_wait_us[self.order].fetch_max(waited, Ordering::Relaxed);
        }
    }
}

impl Future for Watched<'_> {
    type Output = Option<Buffer>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Buffer>> {
        let this = &mut *self;
        let Some(inner) = this.inner.as_mut() else {
            return Poll::Ready(None);
        };
        if this.cancel_after_wake
            && this.parked_at.is_some()
            && this.flag.woken.load(Ordering::Acquire)
        {
            this.inner = None;
            this.unpark();
            this.ledger.cancelled.fetch_add(1, Ordering::Relaxed);
            return Poll::Ready(None);
        }
        *this.flag.task.lock().unwrap() = Some(cx.waker().clone());
        this.flag.woken.store(false, Ordering::Release);
        match inner.as_mut().poll(&mut Context::from_waker(&this.waker)) {
            Poll::Ready(buf) => {
                this.inner = None;
                this.unpark();
                Poll::Ready(Some(buf))
            }
            Poll::Pending => {
                if this.parked_at.is_none() {
                    this.parked_at = Some(Instant::now());
                    this.ledger.parked[this.order].fetch_add(1, Ordering::AcqRel);
                }
                Poll::Pending
            }
        }
    }
}

impl Drop for Watched<'_> {
    fn drop(&mut self) {
        if self.inner.is_some() && self.parked_at.is_some() {
            self.ledger.cancelled.fetch_add(1, Ordering::Relaxed);
        }
        self.unpark();
    }
}

/// Snapshot of the metrics both arena types expose.
struct Snapshot {
    allocations_ok: u64,
    allocations_failed: u64,
    frees: u64,
    frozen: u64,
    spills: u64,
    bytes_live: usize,
}

trait SoakArena: Clone + Send + Sync + 'static {
    const NAME: &'static str;

    fn build(units: usize) -> Self;
    fn capacity(&self) -> usize;
    /// Request size and order for an operation. The order is the index of the
    /// parked count.
    fn pick_request(&self, rng: &mut Rng) -> (usize, usize);
    fn alloc(&self, len: usize) -> BoxFuture<'_, Buffer>;
    fn try_alloc(&self, len: usize) -> Option<Buffer>;
    fn snapshot(&self) -> Snapshot;
    /// Whether a free slot or block large enough for `order` exists now.
    fn serves_order(&self, order: usize) -> bool;
    /// Allocate all capacity in minimum units. For a buddy arena, also make sure
    /// that the whole arena is one block again. Returns the number of allocated
    /// units.
    fn allocate_fully(&self) -> usize;
}

impl SoakArena for AsyncFixedArena {
    const NAME: &'static str = "fixed";

    fn build(units: usize) -> Self {
        FixedArena::with_slot_capacity(nz(units), nz(UNIT))
            .build_async()
            .unwrap()
    }

    fn capacity(&self) -> usize {
        self.slot_count() * self.slot_capacity()
    }

    fn pick_request(&self, _rng: &mut Rng) -> (usize, usize) {
        (UNIT, 0)
    }

    fn alloc(&self, _len: usize) -> BoxFuture<'_, Buffer> {
        Box::pin(self.allocate_async())
    }

    fn try_alloc(&self, _len: usize) -> Option<Buffer> {
        self.allocate().ok()
    }

    fn snapshot(&self) -> Snapshot {
        let m = self.metrics();
        Snapshot {
            allocations_ok: m.allocations_ok,
            allocations_failed: m.allocations_failed,
            frees: m.frees,
            frozen: m.frozen,
            spills: m.spills,
            bytes_live: m.bytes_live,
        }
    }

    fn serves_order(&self, _order: usize) -> bool {
        self.metrics().bytes_live < self.capacity()
    }

    fn allocate_fully(&self) -> usize {
        let mut held = Vec::new();
        while let Ok(buf) = self.allocate() {
            held.push(buf);
        }
        held.len()
    }
}

impl SoakArena for AsyncBuddyArena {
    const NAME: &'static str = "buddy";

    fn build(units: usize) -> Self {
        BuddyArena::builder(BuddyGeometry::exact(nz(units * UNIT), nz(UNIT)).unwrap())
            .build_async()
            .unwrap()
    }

    fn capacity(&self) -> usize {
        self.total_size()
    }

    fn pick_request(&self, rng: &mut Rng) -> (usize, usize) {
        let order = rng.below(MAX_REQUEST_ORDER.min(self.max_order()) as u64 + 1) as usize;
        let size = UNIT << order;
        let len = if order == 0 {
            1 + rng.below(UNIT as u64) as usize
        } else {
            size / 2 + 1 + rng.below(size as u64 / 2) as usize
        };
        (len, order)
    }

    fn alloc(&self, len: usize) -> BoxFuture<'_, Buffer> {
        Box::pin(async move { self.allocate_async(nz(len)).await.unwrap() })
    }

    fn try_alloc(&self, len: usize) -> Option<Buffer> {
        self.allocate(nz(len)).ok()
    }

    fn snapshot(&self) -> Snapshot {
        let m = self.metrics();
        Snapshot {
            allocations_ok: m.allocations_ok,
            allocations_failed: m.allocations_failed,
            frees: m.frees,
            frozen: m.frozen,
            spills: m.spills,
            bytes_live: m.bytes_live,
        }
    }

    fn serves_order(&self, order: usize) -> bool {
        self.metrics().largest_free_block >= UNIT << order
    }

    fn allocate_fully(&self) -> usize {
        assert_eq!(self.metrics().largest_free_block, self.total_size());
        drop(self.allocate(nz(self.total_size())).expect("whole arena"));
        let mut held = Vec::new();
        while let Ok(buf) = self.allocate(nz(UNIT)) {
            held.push(buf);
        }
        held.len()
    }
}

/// The run of operations of one task.
async fn worker<A: SoakArena>(arena: A, ledger: Arc<Ledger>, mut rng: Rng, ops: usize) {
    for _ in 0..ops {
        let (len, order) = arena.pick_request(&mut rng);
        let freeze = rng.chance(70);
        let hold_yields = rng.below(4);
        let hold_sleep = rng.chance(15);

        if rng.chance(25) {
            match arena.try_alloc(len) {
                Some(buf) => {
                    let owned = Owned::new(&ledger, buf, freeze);
                    hold(hold_yields, hold_sleep).await;
                    drop(owned);
                }
                None => {
                    ledger.sync_failed.fetch_add(1, Ordering::Relaxed);
                    tokio::task::yield_now().await;
                }
            }
            continue;
        }

        let cancel = rng.below(100);
        let deadline = Duration::from_millis(1 + rng.below(3));
        if cancel < 10 {
            // The worker creates the future and drops it with no poll.
            drop(arena.alloc(len));
            continue;
        }
        let watched = Watched::new(arena.alloc(len), &ledger, order, (10..25).contains(&cancel));
        let buf = if (25..45).contains(&cancel) {
            tokio::time::timeout(deadline, watched)
                .await
                .unwrap_or_default()
        } else {
            watched.await
        };
        let Some(buf) = buf else {
            continue;
        };
        let owned = Owned::new(&ledger, buf, freeze);
        if (45..55).contains(&cancel) {
            // The deadline occurs while the task has the buffer, and the
            // cancellation drops the buffer.
            let _ = tokio::time::timeout(deadline, async move {
                let _owned = owned;
                tokio::time::sleep(deadline * 4).await;
            })
            .await;
        } else {
            hold(hold_yields, hold_sleep).await;
            drop(owned);
        }
    }
}

async fn hold(yields: u64, sleep: bool) {
    for _ in 0..yields {
        tokio::task::yield_now().await;
    }
    if sleep {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// Samples the arena while the workers run. Fails when the metrics do not agree
/// with the ledger. Also fails when a waiter stays parked next to enough free
/// memory and no allocation succeeds for longer than the stall limit.
async fn watchdog<A: SoakArena>(
    arena: A,
    ledger: Arc<Ledger>,
    stall: Duration,
    stop: Arc<AtomicBool>,
) {
    let mut stalled_since: Option<(Instant, u64)> = None;
    while !stop.load(Ordering::Acquire) {
        let acquired = ledger.acquired.load(Ordering::Acquire);
        let released = ledger.released.load(Ordering::Acquire);
        let m = arena.snapshot();
        if m.allocations_ok < acquired {
            ledger.violation(format!(
                "allocations_ok {} behind {acquired} observed acquisitions",
                m.allocations_ok
            ));
        }
        if m.frees < released {
            ledger.violation(format!(
                "frees {} behind {released} observed releases",
                m.frees
            ));
        }

        let stuck = ledger
            .smallest_parked_order()
            .is_some_and(|order| arena.serves_order(order));
        let now = Instant::now();
        stalled_since = match (stuck, stalled_since) {
            (false, _) => None,
            (true, Some((since, at))) if at == acquired => {
                if now - since > stall {
                    ledger.violation(format!(
                        "waiter parked for {:?} with free memory for it and no progress \
                         (parked per order {:?}, bytes_live {})",
                        now - since,
                        ledger
                            .parked
                            .iter()
                            .map(|p| p.load(Ordering::Relaxed))
                            .collect::<Vec<_>>(),
                        m.bytes_live
                    ));
                    return;
                }
                Some((since, at))
            }
            (true, _) => Some((now, acquired)),
        };
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

struct SeedReport<'a> {
    arena: &'a str,
    seed: u64,
}

impl Drop for SeedReport<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!(
                "async soak failed: arena={} seed={} (replay with ARENA_SOAK_SEED={})",
                self.arena, self.seed, self.seed
            );
        }
    }
}

fn soak_seed<A: SoakArena>(config: Config, seed: u64) {
    let _report = SeedReport {
        arena: A::NAME,
        seed,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(config.threads)
        .enable_all()
        .build()
        .unwrap();
    let arena = A::build(config.units);
    let ledger = Arc::new(Ledger::default());
    let started = Instant::now();
    let mut rounds = 0u64;

    runtime.block_on(async {
        let stop = Arc::new(AtomicBool::new(false));
        let mut dog = tokio::spawn(watchdog(
            arena.clone(),
            Arc::clone(&ledger),
            config.stall,
            Arc::clone(&stop),
        ));
        // Each round ends when all workers finish. At that point, no subsequent
        // free occurs, so a lost wake cannot stay hidden.
        while rounds < config.rounds || started.elapsed().as_secs() < config.secs {
            let workers: Vec<_> = (0..config.tasks)
                .map(|task| {
                    let stream = rounds * config.tasks as u64 + task as u64;
                    tokio::spawn(worker(
                        arena.clone(),
                        Arc::clone(&ledger),
                        Rng::new(seed, stream),
                        config.ops,
                    ))
                })
                .collect();
            let joined = tokio::time::timeout(LIVENESS_DEADLINE, async {
                for handle in workers {
                    handle.await.expect("worker panicked");
                }
            });
            rounds += 1;
            let parked = || -> Vec<usize> {
                ledger
                    .parked
                    .iter()
                    .map(|p| p.load(Ordering::Relaxed))
                    .collect()
            };
            tokio::select! {
                joined = joined => if joined.is_err() {
                    panic!(
                        "workers did not finish within {LIVENESS_DEADLINE:?} \
                         (parked per order {:?}, bytes_live {})",
                        parked(),
                        arena.snapshot().bytes_live
                    );
                },
                _ = &mut dog => panic!(
                    "watchdog stopped in round {rounds}: {:#?}",
                    ledger.violations.lock().unwrap()
                ),
            }
        }
        stop.store(true, Ordering::Release);
        dog.await.unwrap();
    });

    let violations = ledger.violations.lock().unwrap();
    assert!(violations.is_empty(), "violations: {violations:#?}");
    drop(violations);
    assert!(!ledger.any_parked(), "a waiter is still counted as parked");
    assert!(ledger.owners.lock().unwrap().is_empty());

    let acquired = ledger.acquired.load(Ordering::Relaxed);
    let m = arena.snapshot();
    assert_eq!(m.bytes_live, 0, "capacity still accounted as live");
    assert_eq!(m.allocations_ok, acquired, "allocations_ok vs observed");
    assert_eq!(m.frees, acquired, "frees vs observed");
    assert_eq!(m.frozen, ledger.frozen.load(Ordering::Relaxed));
    assert_eq!(m.spills, 0);
    assert!(m.allocations_failed >= ledger.sync_failed.load(Ordering::Relaxed));
    assert_eq!(arena.allocate_fully(), config.units, "capacity lost");

    if std::env::var_os("ARENA_SOAK_VERBOSE").is_some() {
        eprintln!(
            "{} seed={seed} rounds={rounds} acquired={acquired} sync_failed={} cancelled={} \
             allocations_failed={} longest_wait_us_per_order={:?} elapsed={:?}",
            A::NAME,
            ledger.sync_failed.load(Ordering::Relaxed),
            ledger.cancelled.load(Ordering::Relaxed),
            m.allocations_failed,
            ledger
                .longest_wait_us
                .iter()
                .map(|w| w.load(Ordering::Relaxed))
                .collect::<Vec<_>>(),
            started.elapsed()
        );
    }
}

fn soak<A: SoakArena>() {
    let config = Config::from_env();
    for seed in Config::seeds() {
        soak_seed::<A>(config, seed);
    }
}

#[test]
#[cfg_attr(miri, ignore = "multi-threaded soak; too slow under Miri")]
fn fixed_cancellation_soak() {
    soak::<AsyncFixedArena>();
}

#[test]
#[cfg_attr(miri, ignore = "multi-threaded soak; too slow under Miri")]
fn buddy_cancellation_soak() {
    soak::<AsyncBuddyArena>();
}
