#![cfg(all(feature = "semaphore", feature = "evict", feature = "semaphore-total"))]
//! This is an integration/stress test which spawns many jobs in paralel whose job is to take
//! <random amount up TOTAL_RESOURCES> permits from all of the semaphores and give them to an owned
//! one.
//!
//! Timing values are largely useless / inconsistent (+/- 5ms).  More uesful for testing than
//! timing.
use std::{
    any::type_name,
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use async_priority_lock::*;
use futures::future::join_all;
use queue::*;
use rand::{
    RngExt, SeedableRng,
    rngs::SmallRng,
    seq::{IndexedRandom, SliceRandom},
};
use tokio::{
    sync::{SetOnce, broadcast},
    task::JoinSet,
};

const POOL_SIZE: usize = 0x100;
const INITIAL_RESOURCE_COUNT: usize = 64;
const MOVE_JOB_COUNT: usize = 0x200;
const RAND_JOB_COUNT: usize = 0x00;
const JOB_COUNT: usize = MOVE_JOB_COUNT + RAND_JOB_COUNT;

trait TestPrio: Priority + From<usize> + core::fmt::Debug + Clone + Send + Sync + 'static {}
impl<T: Priority + From<usize> + core::fmt::Debug + Clone + Send + Sync + 'static> TestPrio for T {}

#[derive(Debug)]
struct BarrierThatIsNotTrash {
    tx: SetOnce<()>,
    tgt: AtomicUsize,
}

impl BarrierThatIsNotTrash {
    fn new(tgt: usize) -> Self {
        Self {
            tx: Default::default(),
            tgt: AtomicUsize::new(tgt),
        }
    }

    async fn wait(&self, count: usize) {
        if self.tx.initialized() {
            return;
        }

        if count == 0 {
            self.tx.wait().await;
            return;
        }

        if self.tgt.fetch_sub(count, Ordering::SeqCst) <= count {
            _ = self.tx.set(());
            return;
        }

        let _guard = NotTrashGuard(self, count);

        self.tx.wait().await;
        // some optimization here - no need to increase counter here once we're done
        // (gaurd is to make this cancel safe)
        std::mem::forget(_guard);
    }

    fn ready(&self) -> bool {
        return self.tx.initialized();
    }
}

struct NotTrashGuard<'a>(&'a BarrierThatIsNotTrash, usize);

impl<'a> Drop for NotTrashGuard<'a> {
    fn drop(&mut self) {
        self.0.tgt.fetch_add(self.1, Ordering::SeqCst);
    }
}

static BASE_SEED: AtomicU64 = AtomicU64::new(0);

async fn take_resources<'a, P: TestPrio, Q: SemaphoreQueue<P>>(
    sem: &'a Semaphore<P, Q>,
    priority: usize,
    bar: &BarrierThatIsNotTrash,
    needed: usize,
) -> Option<(usize, SemaphorePermit<'a, P, Q>)> {
    loop {
        let total = sem.total_permits();
        if total == 0 {
            tokio::select! {
                _ = bar.wait(0) => { return None },
                // lazy code here to avoid using a timeout...
                _ = sem.acquire(priority.into()) => continue,
            }
        }

        let to_take = total.min(needed);

        let mut permit = tokio::select! {
            _ = bar.wait(0) => { return None },
            x = sem.acquire_within_total(priority.into(), to_take) => match x {
                Ok(y) => y,
                Err(_) => {
                    continue;
                },
            },
        };

        tokio::select! {
            _ = sem.acquire(priority.into()) => { if !bar.ready() { continue }},
            _ = permit.evicted() => { if !bar.ready() { continue } },
            _ = bar.wait(to_take) => {},
        };

        return Some((to_take, permit));
    }
}

async fn take_random<P: TestPrio, Q: SemaphoreQueue<P>>(
    resources: Arc<Vec<Semaphore<P, Q>>>,
    mut done: broadcast::Receiver<()>,
    wait: Arc<BarrierThatIsNotTrash>,
    id: u64,
) {
    wait.wait(1).await;
    drop(wait);
    let mut rng = SmallRng::seed_from_u64(BASE_SEED.load(Ordering::Relaxed) ^ id);
    let delay = rng.random_range(0..8000);

    tokio::time::sleep(Duration::from_nanos(delay)).await;
    let mut intv = tokio::time::interval(Duration::from_micros(8));
    loop {
        tokio::select! {
            _ = intv.tick() => { },
            _ = done.recv() => return,
        };

        let sem = { resources.choose(&mut rng).unwrap() };

        tokio::select! {
            _ = sem.acquire_within_total(usize::MAX.into(), 1) => continue,
            _ = done.recv() => return,
            _ = tokio::time::sleep(Duration::from_micros(8)) => continue,
        };
    }
}

async fn collect_resources<P: TestPrio, Q: SemaphoreQueue<P>>(
    resources: Arc<Vec<Semaphore<P, Q>>>,
    priority: usize,
    wait: Arc<BarrierThatIsNotTrash>,
) {
    wait.wait(1).await;
    drop(wait);

    let mut rng = SmallRng::seed_from_u64(priority as u64 ^ BASE_SEED.load(Ordering::Relaxed));
    // need to define these in the following scope since they rely on rng (which we can't hold
    // across await)
    let mut resources_needed = rng.random_range(1..=(INITIAL_RESOURCE_COUNT * 8));
    let bar = BarrierThatIsNotTrash::new(resources_needed);

    let res = resources.choose(&mut rng).unwrap();

    let permits = {
        let mut items: Vec<_> = resources.iter().collect();
        items.shuffle(&mut rng);

        join_all(
            items
                .into_iter()
                .map(|x| take_resources(x, priority, &bar, resources_needed)),
        )
    };

    for (count, mut permit) in permits.await.into_iter().filter_map(|x| x) {
        if permit.belongs_to(res) {
            if count >= resources_needed {
                break;
            }

            resources_needed -= count;
            drop(permit);

            // drop permit in place instead of forgetting -> adding
            // this helps us test more + also prevents incorrectly having a total amount of permits
            // greater than TOTAL_RESOURCES
            continue;
        }

        if count > resources_needed {
            permit
                .split_with_priority(resources_needed, priority.into())
                .unwrap()
                .forget();

            drop(permit);
            res.add_permits(resources_needed);
            break;
        }

        permit.forget();
        res.add_permits(count);
        if resources_needed <= count {
            break;
        }
        resources_needed -= count;
        if resources_needed == 0 {
            break;
        }
    }
}

async fn test<P: 'static + TestPrio, Q: 'static + SemaphoreQueue<P> + Sync + Send>()
-> (&'static str, Duration)
where
    Q::Handle: Sync + Send,
{
    let mut resources = Vec::with_capacity(POOL_SIZE);
    resources.resize_with(POOL_SIZE, || Semaphore::<P, Q>::new(INITIAL_RESOURCE_COUNT));

    let barrier = Arc::new(BarrierThatIsNotTrash::new(JOB_COUNT + 1));

    let resources = Arc::new(resources);

    let jobs = JoinSet::from_iter(
        (0..MOVE_JOB_COUNT).map(|x| collect_resources(resources.clone(), x, barrier.clone())),
    );

    let (done_tx, done_rx) = broadcast::channel(1);

    let rand_jobs = JoinSet::from_iter((0..RAND_JOB_COUNT).map(|id| {
        take_random(
            resources.clone(),
            done_rx.resubscribe(),
            barrier.clone(),
            id as u64,
        )
    }));

    barrier.wait(1).await;
    let start = Instant::now();

    _ = jobs.join_all().await;
    drop(done_tx);
    _ = rand_jobs.join_all().await;

    (type_name::<Q>(), start.elapsed())
}

macro_rules! test_prios {
    ($i: ident, $q: ty) => {
        match $i % 4 {
            0 => test::<usize, $q>().await,
            1 => test::<LowestFirst<usize>, $q>().await,
            2 => test::<FIFO<usize>, $q>().await,
            3 => test::<LIFO<usize>, $q>().await,
            _ => unreachable!(),
        }
    };
}

#[tokio::test(flavor = "multi_thread")]
pub async fn run() {
    // Second value is for a custom measurement which is added manually around the functions which
    // should be timed.  code added when needed.
    let mut results: BTreeMap<&'static str, [Duration; 2]> = BTreeMap::new();

    const PRIO_COUNT: usize = 4;
    const QUEUE_COUNT: usize = 4;
    const BRANCH_COUNT: usize = PRIO_COUNT * QUEUE_COUNT;
    const ROUND_COUNT: usize = 64;
    const TEST_COUNT: usize = ROUND_COUNT * BRANCH_COUNT;

    for i in 0..TEST_COUNT {
        if i % BRANCH_COUNT == 0 {
            BASE_SEED.store(rand::rng().random(), Ordering::Relaxed);
        }

        let (name, delay) = match (i / PRIO_COUNT) % QUEUE_COUNT {
            1 => test_prios!(i, SingleLinkArenaQueue<_>),
            // 1 => test_prios!(i, SingleLinkArenaQueue<_>),
            // 2 => test_prios!(i, SingleLinkArenaQueue<_>),
            // 3 => test_prios!(i, SingleLinkArenaQueue<_>),
            0 => test_prios!(i, DualLinkArenaQueue<_>),
            2 => test_prios!(i, SingleLinkBoxQueue<_>),
            3 => test_prios!(i, DualLinkBoxQueue<_>),
            _ => unreachable!(),
        };

        let entry = results.entry(name).or_default();
        entry[0] += delay;
        let avg = Duration::from_secs(0);
        entry[1] += avg;

        println!(
            "run {: <4} {:?} {:?} - {}",
            i,
            delay,
            avg,
            name.replace("async_priority_lock::", "")
                .replace("queue::", "")
                .replace("arena::", "")
                .replace("semaphore::", ""),
        );
    }

    for (i, [dur, mes]) in results {
        println!(
            "avg: {: >12?} (total: {: >12?}) | measured avg: {: >12?} total: {: >12?} - {}",
            dur / (ROUND_COUNT as u32),
            dur,
            mes / (ROUND_COUNT as u32),
            mes,
            i.replace("async_priority_lock::", "")
                .replace("queue::", "")
        )
    }
}
