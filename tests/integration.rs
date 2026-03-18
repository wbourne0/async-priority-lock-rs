#![cfg(feature = "evict")]
use async_priority_lock::*;
use queue::*;
use std::{
    alloc::GlobalAlloc,
    collections::{BTreeMap, HashSet},
    fmt::Debug,
    marker::Send,
    sync::{
        Arc, Mutex as SyncMutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{self, Duration},
};

use anyhow::{Result, anyhow};
use futures::future::join_all;
use rand::{RngExt, seq::IndexedRandom};
use tokio::{
    select,
    sync::{SetOnce, broadcast},
    task::JoinSet,
    time::Instant,
};

struct StatAllocator<G: GlobalAlloc>(G);
static TOTAL_ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);
static MAX_ALLOCATED: AtomicUsize = AtomicUsize::new(0);

/// While we don't make any unsafe allocations ourselves, we *do* have some specialized logic to
/// drop wakers, and if we faill to drop them it's likely it would leak memory.
/// So to be safe, we track allocation count and bytes allocated and check at the end.
#[global_allocator]
static ALLOC: StatAllocator<std::alloc::System> = StatAllocator(std::alloc::System);

#[inline]
fn set_max(curr: usize) {
    loop {
        let max = MAX_ALLOCATED.load(Ordering::Relaxed);
        if max >= curr {
            return;
        }

        if MAX_ALLOCATED
            .compare_exchange(max, curr, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
    }
}

unsafe impl<G: GlobalAlloc> GlobalAlloc for StatAllocator<G> {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        set_max(ALLOC_COUNT.fetch_add(1, Ordering::Relaxed));
        TOTAL_ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);

        unsafe { self.0.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        ALLOC_COUNT.fetch_sub(1, Ordering::Relaxed);
        TOTAL_ALLOCATED.fetch_sub(layout.size(), Ordering::Relaxed);

        unsafe { self.0.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        set_max(ALLOC_COUNT.fetch_add(1, Ordering::Relaxed));
        TOTAL_ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { self.0.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        if layout.size() > new_size {
            TOTAL_ALLOCATED.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
        } else {
            TOTAL_ALLOCATED.fetch_add(new_size - layout.size(), Ordering::Relaxed);
        }

        unsafe { self.0.realloc(ptr, layout, new_size) }
    }
}

#[repr(C)]
struct Resource<P: Priority + Default, Q: MutexQueue<P>> {
    intent: Mutex<P, SyncMutex<Vec<usize>>, Q>,
}

impl<P: Priority + Default, Q: MutexQueue<P>> Default for Resource<P, Q> {
    fn default() -> Self {
        Self {
            intent: Default::default(),
        }
    }
}

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

    async fn wait(&self) {
        if self.tx.initialized() {
            return;
        }
        if self.tgt.fetch_sub(1, Ordering::SeqCst) == 1 {
            _ = self.tx.set(());
            return;
        }
        let _guard = NotTrashGuard(self);

        self.tx.wait().await;
        // some optimization here - no need to increase counter here once we're done
        // (gaurd is to make this cancel safe)
        std::mem::forget(_guard);
    }

    fn ready(&self) -> bool {
        return self.tx.initialized();
    }
}

struct NotTrashGuard<'a>(&'a BarrierThatIsNotTrash);

impl<'a> Drop for NotTrashGuard<'a> {
    fn drop(&mut self) {
        self.0.tgt.fetch_add(1, Ordering::SeqCst);
    }
}

impl<P: Priority + From<usize> + Default + Debug, Q: MutexQueue<P>> Resource<P, Q> {
    async fn get(&self, bar: &BarrierThatIsNotTrash, holder: usize) {
        loop {
            LOCK_COUNT.fetch_add(1, Ordering::Relaxed);
            let mut intent = self.intent.lock_from(holder).await;

            select! {
                _ = bar.wait() => {},
                _ = MutexGuard::evicted(&mut intent) => {
                    if !bar.ready() {
                        EVICT_COUNT.fetch_add(1, Ordering::SeqCst);
                        continue
                    }
                },
            }

            let x = &*intent;
            // we use try_lock with unwrap here to ensure we panic in case we accidentally give
            // multiple guards
            x.try_lock().unwrap().push(holder);

            return;
        }
    }

    async fn get_no_wait(&self, holder: usize) {
        LOCK_COUNT.fetch_add(1, Ordering::Relaxed);
        let guard = self.intent.lock_from(holder).await;

        // w; use try_lock with unwrap here to ensure we panic in case we accidentally give
        // multiple guards
        guard.try_lock().unwrap().push(holder)
    }
}

async fn run_seq_job<P: Priority + Default + From<usize> + Debug, Q: MutexQueue<P>>(
    id: usize,
    bar: Arc<tokio::sync::Barrier>,
    res: Vec<Arc<Resource<P, Q>>>,

    _exit: broadcast::Sender<()>,
) {
    bar.wait().await;
    let subbar = Arc::new(BarrierThatIsNotTrash::new(res.len()));

    select! {

        _ = join_all(res.into_iter().map(|x| async {
        // explicitly move x...
        let bruh = x;
        bruh.get(&subbar, id).await
    }))
     => {},
        _ = subbar.tx.wait() => {},

    };
}

static LOCK_COUNT: AtomicUsize = AtomicUsize::new(0);
static EVICT_COUNT: AtomicUsize = AtomicUsize::new(0);

const RESOURCE_COUNT: usize = 0x80000;
// const RESOURCE_COUNT: usize = 1;
// Count of jobs that immediately get a random resource every microsecond until multi jobs complete
const RAND_JOB_COUNT: usize = 128;
// Count of jobs which wait until they acquire RESOURCES_USED distinct resources (evicting until
// all are acquired)
const MULTI_RES_JOB_COUNT: usize = 0x2000;
const JOB_COUNT: usize = RAND_JOB_COUNT + MULTI_RES_JOB_COUNT;
/// Count of resources each multi res job acquires
const RESOURCES_USED: usize = 64;
// const RESOURCES_USED: usize = 1;

static WTF: AtomicU64 = AtomicU64::new(0);
fn seq_job<P: 'static + Priority + Default + From<usize> + Debug, Q: 'static + MutexQueue<P>>(
    id: usize,
    bar: &Arc<tokio::sync::Barrier>,
    all_resources: &Vec<Arc<Resource<P, Q>>>,
    exit: broadcast::Sender<()>,
) -> impl Future<Output = ()> + 'static {
    let mut rng = rand::rng();
    let now = Instant::now();

    let resources: Vec<_> = (&*all_resources)
        .sample(&mut rng, RESOURCES_USED)
        .into_iter()
        .map(|x| x.clone())
        .collect();

    WTF.fetch_add(
        Instant::now().duration_since(now).as_nanos() as u64,
        Ordering::Relaxed,
    );

    run_seq_job(id, bar.clone(), resources, exit)
}

async fn step_job<
    const PRIO: usize,
    P: Priority + Default + From<usize> + Debug,
    Q: MutexQueue<P>,
>(
    start: Arc<tokio::sync::Barrier>,
    mut exit: broadcast::Receiver<()>,
    resources: Arc<Vec<Arc<Resource<P, Q>>>>,
) {
    start.wait().await;
    loop {
        let idx = rand::rng().random_range(0..resources.len());

        resources[idx].get_no_wait(PRIO).await;
        select! {
            // wait without changing waiter count
            _ = exit.recv() => { return },
            _ = tokio::time::sleep(std::time::Duration::from_micros(1)) => {},
        };
    }
}

// fn test(x: impl Send + Sync) {}
// fn test_<'a, const B: bool>(x: MutexGuard<'a, usize, std::rc::Rc<usize>, B>) {
//     test(x)
// }
//
// #[tokio::main]
async fn fun<
    P: Priority + From<usize> + Default + Send + 'static + Debug,
    Q: 'static + Sync + Send + MutexQueue<P>,
    const STEP_PRIO: usize,
>() -> Result<(&'static str, time::Duration)>
where
    Q::Handle: Send + Sync,
{
    let mut items = Vec::with_capacity(RESOURCE_COUNT);
    items.resize_with(RESOURCE_COUNT, Arc::<Resource<P, Q>>::default);
    let resources = Arc::new(items);

    let barier = Arc::new(tokio::sync::Barrier::new(JOB_COUNT + 1));

    let (exit_tx, exit_rx) = broadcast::channel(1);

    let set = JoinSet::from_iter((0..JOB_COUNT).map(|job| {
        let bruh = if job < MULTI_RES_JOB_COUNT {
            Ok(seq_job(job, &barier, &resources, exit_tx.clone()))
        } else {
            Err(step_job::<STEP_PRIO, P, Q>(
                barier.clone(),
                exit_rx.resubscribe(),
                resources.clone(),
            ))
        };
        async move {
            match bruh {
                Ok(x) => x.await,
                Err(x) => x.await,
            }
        }
    }));

    // allow step jobs to exit
    drop(exit_tx);
    let start = Instant::now();

    barier.wait().await;
    set.join_all().await;

    let time = Instant::now().duration_since(start);

    let evicts = EVICT_COUNT.swap(0, Ordering::Relaxed);
    // This is fairly arbitray, and technically we could be ok even if it was zero.
    // but we expect there to be quite a few evicts with the current set
    // (well over 25k).  If we're not getting many evictions, then there's something wrong with the
    // testing appoach - as we want to test everything many times.
    if evicts < RAND_JOB_COUNT * 5 {
        return Err(anyhow!("unexpectedly small evict count: {} ", evicts));
    }

    let mut map: HashSet<(usize, usize)> =
        HashSet::with_capacity(MULTI_RES_JOB_COUNT * MULTI_RES_JOB_COUNT);

    let mut direction: isize = 0;

    // PERF: Could make this multithreaded / split into batches then merge.  This is by far the
    // slowest part of the test.
    for res in resources.iter() {
        let outer = res.intent.lock_from(0).await;
        let mut l = outer.lock().unwrap();

        for (i, x) in l.iter().enumerate() {
            if *x == STEP_PRIO {
                continue;
            }
            for y in &l[(i + 1)..] {
                if *y == STEP_PRIO {
                    continue;
                }
                map.insert((*x, *y));

                if map.contains(&(*y, *x)) {
                    return Err(anyhow!(
                        "received instance of {x} -> {y} when previous case of {y} -> {x} exists"
                    ));
                }

                if x > y {
                    direction += 1
                } else {
                    direction -= 1
                }
            }
        }

        l.clear();
    }

    let lowest_first = P::from(0).compare(&1.into()).is_gt();

    // Again here this number is fairly arbitrary.  It's technically not wrong for all of the
    // lowest priority locks to run in order (e.g. all of the low ones happen to run and complete
    // before higher priority ones) but again this doesn't really test for race cases, hence we
    // want to ensure we get a reasonably high bias.
    const SEQ_CHANGES: isize = (MULTI_RES_JOB_COUNT) as isize;
    if lowest_first {
        if direction >= -(SEQ_CHANGES) {
            return Err(anyhow!(
                "bias does not represent enough high -> low priority {} (wanted {})",
                direction,
                -SEQ_CHANGES,
            ));
        }
    } else if direction < SEQ_CHANGES {
        return Err(anyhow!(
            "bias does not represent enough high -> low priority {} (wanted {})",
            direction,
            SEQ_CHANGES,
        ));
    }

    Ok((core::any::type_name::<Q>(), time))
}

type BoxMutexQueue<P> = SingleLinkBoxQueue<MutexWaiter<P>>;

macro_rules! test_prios {
    ($i: ident, $q: ty) => {
        match $i % 4 {
            0 => fun::<usize, $q, 0>().await,
            1 => fun::<LowestFirst<usize>, $q, 0>().await,
            2 => fun::<FIFO<usize>, $q, 0>().await,
            3 => fun::<LIFO<usize>, $q, 0>().await,
            _ => unreachable!(),
        }
    };
}

/// Will fail on 1 threaded machines
#[tokio::test(flavor = "multi_thread")]
pub async fn run() -> Result<()> {
    // println also appears to have some state it lazily initializes, so here we're making sure
    // that is initialized

    println!("hi :)");
    // callnig this function once calls some state to be lazily initialized in tokio, and since we
    // track allocations we need to measure after those have been initialized once.
    fun::<usize, BoxMutexQueue<usize>, { usize::MIN }>().await?;

    let start_alloc_bytes = TOTAL_ALLOCATED.load(Ordering::Relaxed);
    let start_alloc_count = ALLOC_COUNT.load(Ordering::Relaxed);

    {
        // Second value is for a custom measurement which is added manually around the functions which
        // should be timed.  code added when needed.
        let mut results: BTreeMap<&'static str, [Duration; 2]> = BTreeMap::new();

        const PRIO_COUNT: usize = 4;
        const QUEUE_COUNT: usize = 4;
        const BRANCH_COUNT: usize = PRIO_COUNT * QUEUE_COUNT;

        const ROUND_COUNT: usize = 32;
        const TEST_COUNT: usize = ROUND_COUNT * BRANCH_COUNT;
        LOCK_COUNT.swap(0, Ordering::Relaxed);

        for i in 0..TEST_COUNT {
            // pre sem:
            // arena  true | high priority inter: false : avg  60.525399ms total 7.747251044s
            // arena  true | high priority inter:  true : avg  60.631249ms total 7.760799844s
            // arena false | high priority inter: false : avg  88.078902ms total 11.274099413s
            // arena false | high priority inter:  true : avg  86.885806ms total 11.121383207s
            let (name, delay) = match (i / PRIO_COUNT) % 4 {
                0 => test_prios!(i, SingleLinkArenaQueue<_>),
                // 1 => test_prios!(i, SingleLinkArenaQueue<_>),
                // 2 => test_prios!(i, SingleLinkArenaQueue<_>),
                // 3 => test_prios!(i, SingleLinkArenaQueue<_>),
                1 => test_prios!(i, DualLinkArenaQueue<_>),
                2 => test_prios!(i, SingleLinkBoxQueue<_>),
                3 => test_prios!(i, DualLinkBoxQueue<_>),
                _ => unreachable!(),
            }?;

            let entry = results.entry(name).or_default();
            entry[0] += delay;
            let avg = Duration::default();
            entry[1] += avg;

            println!(
                "run {: <4} {:?} {:?} - {}",
                i,
                delay,
                avg,
                name.replace("async_priority_lock::", "")
                    .replace("queue::", "")
                    .replace("arena::", "")
                    .replace("mutex::", ""),
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
                    .replace("arena::", "")
                    .replace("box::", "")
            )
        }
    }

    let end_alloc_count = ALLOC_COUNT.load(Ordering::Relaxed);
    if end_alloc_count != start_alloc_count {
        return Err(anyhow::anyhow!(
            "start alloc count doesn't match end alloc count! start {} end {}",
            start_alloc_count,
            end_alloc_count
        ));
    }

    let end_alloc_bytes = TOTAL_ALLOCATED.load(Ordering::Relaxed);
    if end_alloc_bytes != start_alloc_bytes {
        return Err(anyhow::anyhow!(
            "start alloc bytes doesn't match end alloc bytes! start {} end {}",
            start_alloc_bytes,
            end_alloc_bytes
        ));
    }

    Ok(())
}
