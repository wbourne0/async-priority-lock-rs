use async_priority_lock::Semaphore;
use std::{pin::pin, time::Duration};

#[tokio::main]
async fn main() {
    let sem = Semaphore::<usize>::new(10);

    let permit = sem.acquire(0).await;
    // try to acquire 10 permits
    let mut many_permits_fut = pin!(sem.acquire_within_total(1, 10));
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(1)) => {},
        // can't happen
        _ =  many_permits_fut.as_mut() => { panic!("total of 11 tokens held")},
    };

    // remove 1 permit from the semaphore
    permit.forget();

    assert!(many_permits_fut.await.is_err());
}
