# Changelog

#### v1.0.2
- fix typo in feature table
- relaxed `Sized` bound on `Mutex` `T`

#### v1.0.1
- fix typo in README.md example
- fix `Default` impl for `Semaphore`

#### v1.0.0

- Added `Priority` and `PriorityQueue` traits
- Renamed `PriorityMutex` -> `Mutex` and rewrote to use `Priority` and `PriorityQueue` traits.
- Added `Semaphore`
- Added changelog

