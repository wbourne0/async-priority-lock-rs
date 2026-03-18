# Feature flags:
| Feature | Default | Description
|---|:-:|---|
|`evict` | ❌ | Enables eviction for [Semaphore] and [Mutex].
| `semaphore-cap` | ❌ | Enables [Semaphore::total_permits] and [Semaphore::acquire_within_total]. |
| `serde` | ❌ | Implement [serde::Deserialize] for [Mutex] + builtin [Priority] structs. |
| `box-queue` | ❌ | Enables [queue::boxed]. |
| `arena-queue` | ✅ | Enables [queue::arena]. |
| `const-default` | ✅ | Implement [const_default::ConstDefault] for [Mutex] and [Semaphore].  Also enables `const_new` fns. |
| `mutex` | ✅ | Enable [Mutex]. |
| `semaphore` | ✅ | Enable [Semaphore]. |
| `std` | ✅ | Enables use of [std] (disable for `no_std` envs. note if disabled, `spin` must be enabled). |
| `alloc` | ✅ | Enables use of [alloc] crate.  Always required for `box-queue` and `arena-queue`. |
| `spin` | ❌ | Use [spin] for internal locks (note: [spin] won't be used if `std` flag is enabled). |
