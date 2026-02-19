# async-priority-lock

A priority-based mutex where access is granted by order of priority.

# Example

```rust
use async_priority_lock::{PriorityMutex, PriorityMutexGuard};
use tokio::time::{Duration, sleep};

static MY_LOCK: PriorityMutex<usize, Vec<usize>> = PriorityMutex::new(vec![]);

async fn append(priority: usize, value: usize) {
    loop {
        let mut guard = MY_LOCK.lock(priority).await;

        tokio::select! {
            // Abort and retry on evict.  Note that eviction is voluntary and isn't
            // enabled unless the `evict` flag is set.  If this example didn't handle,
            // evict the result would have a non-deterministic start (whichever got the lock first)
            // with subsequent entries pushing to the list according to their priority
            _ = PriorityMutexGuard::evicted(&mut guard) => { continue },
            // Make sure this future is cancel-safe if running in a loop like this
            _ = sleep(Duration::from_millis(10)) => {
                guard.push(value);
                break;
            }
        }
    }
}

#[tokio::main]
async fn main() {
    tokio::join!(append(0, 1), append(3, 3), append(2, 2));
    println!("data: {:?}", MY_LOCK); // data: PriorityMutex([3, 2, 1])
}
```


# Features
- `evict` - enables eviction of current lock holders `PriorityMutexGuard::evicted`
- `no-std` - allows compiling for `no_std` (note: `alloc` still required)
- `serde` - allows serde to deserialize into mutexes (but not serialize)

