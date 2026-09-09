//! A minimal sharded concurrent hash map for the profiler's own bookkeeping.
//!
//! # Why not DashMap
//!
//! Ying used to use `DashMap` here, via a fork that made the hash table allocate from
//! `std::alloc::System` instead of the global allocator.  That fork existed to break re-entrancy:
//! Ying *is* the global allocator, so anything Ying allocates re-enters `YingProfiler::alloc`.
//!
//! The fork was not enough.  DashMap's `RwLock` is built on `parking_lot_core`, and
//! `parking_lot_core` allocates its global thread-parking table *while holding its own internal
//! bucket locks*.  That allocation lands back in `YingProfiler::alloc`, which touches a DashMap,
//! whose lock slow path calls back into `parking_lot_core` and self-deadlocks on a lock the same
//! thread already holds.  No choice of allocator for the hash table can fix that, because the
//! allocation happens inside the lock implementation rather than inside the map.
//!
//! # Why allocating from the global allocator is fine here
//!
//! Note what this map does *not* do: the `HashMap` inside each shard allocates from the global
//! allocator, which is Ying itself.  That is deliberate, and it is a change from the old fork's
//! approach of side-stepping the global allocator entirely.
//!
//! Re-entering Ying is harmless as long as the re-entrant call cannot touch profiler state.  Two
//! properties together guarantee that:
//!
//! 1. **Every caller holds the thread's allocator lock** for the whole duration of a map access
//!    (see `YingProfiler::lock_out_profiler`).  `alloc` takes the lock before its first map
//!    access; `dealloc` and `realloc` check it and return early *before* touching any map.  So
//!    when a shard resizes and allocates a new table, that nested `alloc` sees the lock already
//!    held, skips all profiling, and does nothing but forward to `System`.  Same for the free of
//!    the old table.  The recursion is exactly one level deep and terminates there.
//! 2. **The locks themselves never allocate.**  This is the property DashMap could not offer.
//!    `std::sync::RwLock` is a futex on Linux, and a queue of stack-allocated nodes on platforms
//!    without one, so acquiring or releasing a shard lock cannot allocate and cannot re-enter Ying
//!    at all — let alone re-enter it at a point where a lock is half-initialized.
//!
//! Property 1 alone is not sufficient, and that is the subtle part: the `parking_lot_core`
//! deadlock happened on a thread that was *not* inside profiler code, so no re-entrancy guard was
//! set and none could have been.  The allocation came from the lock library's own lazy
//! initialization.  Property 2 is what rules that out, and it is why the choice of lock matters
//! more than the choice of allocator.
//!
//! The practical rule for anyone changing this code: profiler state may allocate, but it must
//! never be guarded by a lock that allocates.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::thread::available_parallelism;

/// Upper bound on shard count.  More shards means less contention but a bigger fixed footprint.
const MAX_SHARDS: usize = 128;
const DEFAULT_SHARDS: usize = 16;
const SHARDS_PER_CORE: usize = 4;

type Shard<V> = HashMap<u64, V, BuildHasherDefault<U64Hasher>>;

/// Finalizer from MurmurHash3, applied to keys that are already integers.
#[inline]
fn mix64(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51afd7ed558ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ceb9fe1a85ec53);
    x ^= x >> 33;
    x
}

/// Hasher for `u64` keys.  Keys here are stack hashes and pointers, so a single mixing round is
/// enough and costs far less than a general purpose hasher on the allocation hot path.
#[derive(Default)]
pub(crate) struct U64Hasher(u64);

impl Hasher for U64Hasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = mix64(self.0 ^ *b as u64);
        }
    }

    #[inline]
    fn write_u64(&mut self, n: u64) {
        self.0 = mix64(n);
    }

    #[inline]
    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64);
    }
}

/// A concurrent `u64`-keyed map: a fixed array of `std::sync::RwLock`-guarded [`HashMap`]s.
///
/// # Sharding
///
/// Keys are assigned to a shard by the high 32 bits of [`mix64`], while each shard's `HashMap`
/// buckets off the low bits of the same hash, so the two levels key off different bits and a run
/// of adjacent keys (consecutive heap addresses, say) spreads evenly instead of piling into one
/// shard.  Shard count is fixed at construction: four per CPU, rounded up to a power of two and
/// clamped to `[16, 128]`, so the index is a mask rather than a division.  It never changes, which
/// means a shard reference stays valid for the life of the map and there is no global rehash to
/// synchronize against — individual shards resize independently, under their own lock.
///
/// # Concurrency
///
/// Locking is per shard, so N shards allow up to N concurrent writers and any number of concurrent
/// readers per shard.  There is no cross-shard coordination at all, which is why [`ShardedMap::len`]
/// and iteration are not atomic snapshots: they visit one shard at a time, and entries can be
/// added or removed in shards that have already been visited.  Nothing in the profiler needs a
/// consistent snapshot — reports are inherently approximate — so that tradeoff buys a lock that is
/// only ever held for the duration of a single hash lookup.
///
/// # Why `RwLock` is fast enough here
///
/// The usual objection to lock-per-shard maps is that a mutex is slower than the lock-free
/// alternatives, but that comparison does not apply at this call site:
///
/// - **Ying is a sampling profiler.**  At the default ratio of 1 in 500, only sampled allocations
///   touch `stack_stats`, and the surrounding work (`Backtrace::new_unresolved`, hashing 30 frames)
///   costs far more than an uncontended lock, which is a single atomic compare-exchange.
/// - **Contention is spread across shards.**  On a 16-core machine that is 64 shards, so two
///   threads collide on the same shard only about 1.5% of the time even when both are sampling
///   simultaneously.
/// - **Critical sections are tiny.**  Every operation is one hash lookup plus a field update, so a
///   contended lock is waited on for tens of nanoseconds rather than parked.
///
/// The one exception is the new-stack path in `alloc`, which resolves symbols while holding a
/// shard's write lock and can be slow; see the note at that call site.
///
/// Callers must hold the thread's allocator lock for the duration of any access — see the module
/// documentation for why.
pub(crate) struct ShardedMap<V> {
    shards: Box<[RwLock<Shard<V>>]>,
    shard_mask: usize,
}

impl<V> ShardedMap<V> {
    /// Creates a map preallocated for roughly `capacity` total entries, split across shards.
    pub fn with_capacity(capacity: usize) -> Self {
        let num_shards = available_parallelism()
            .map(|n| n.get() * SHARDS_PER_CORE)
            .unwrap_or(DEFAULT_SHARDS)
            .next_power_of_two()
            .clamp(DEFAULT_SHARDS, MAX_SHARDS);
        let per_shard = capacity.div_ceil(num_shards);

        let shards = (0..num_shards)
            .map(|_| {
                RwLock::new(Shard::with_capacity_and_hasher(
                    per_shard,
                    Default::default(),
                ))
            })
            .collect();

        Self {
            shards,
            shard_mask: num_shards - 1,
        }
    }

    /// Uses the high bits for shard selection so that shard choice and in-shard bucket choice do
    /// not key off the same bits.
    #[inline]
    fn shard_of(&self, key: u64) -> usize {
        (mix64(key) >> 32) as usize & self.shard_mask
    }

    /// A poisoned lock means a panic happened while the map was being mutated.  Panicking here
    /// would abort the process, since this runs inside the global allocator, so recover the data
    /// and carry on with possibly-inconsistent profiling stats instead.
    #[inline]
    fn read(&self, key: u64) -> RwLockReadGuard<'_, Shard<V>> {
        self.shards[self.shard_of(key)]
            .read()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[inline]
    fn write(&self, key: u64) -> RwLockWriteGuard<'_, Shard<V>> {
        self.shards[self.shard_of(key)]
            .write()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[inline]
    pub fn contains_key(&self, key: u64) -> bool {
        self.read(key).contains_key(&key)
    }

    #[inline]
    pub fn insert(&self, key: u64, value: V) -> Option<V> {
        self.write(key).insert(key, value)
    }

    #[inline]
    pub fn remove(&self, key: u64) -> Option<V> {
        self.write(key).remove(&key)
    }

    /// Updates the entry for `key` in place if present, otherwise inserts what `create` returns.
    /// `create` is only called when the key is absent, and runs while the shard is locked.
    #[inline]
    pub fn update_or_insert_with(
        &self,
        key: u64,
        update: impl FnOnce(&mut V),
        create: impl FnOnce() -> V,
    ) {
        let mut shard = self.write(key);
        match shard.get_mut(&key) {
            Some(existing) => update(existing),
            None => {
                shard.insert(key, create());
            }
        }
    }

    /// Updates the entry for `key` in place if it exists; does nothing otherwise.
    #[inline]
    pub fn update(&self, key: u64, update: impl FnOnce(&mut V)) {
        if let Some(existing) = self.write(key).get_mut(&key) {
            update(existing)
        }
    }

    /// Runs `f` against the value for `key` while holding the shard's read lock.  `f` must not
    /// call back into this map, and must not block.
    #[inline]
    pub fn with_value<R>(&self, key: u64, f: impl FnOnce(Option<&V>) -> R) -> R {
        f(self.read(key).get(&key))
    }

    pub fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|s| s.read().unwrap_or_else(|e| e.into_inner()).len())
            .sum()
    }

    pub fn clear(&self) {
        for shard in self.shards.iter() {
            shard.write().unwrap_or_else(|e| e.into_inner()).clear();
        }
    }

    /// Projects every entry through `f` and collects the results, without cloning the values.
    ///
    /// This is the only way to scan the whole map, deliberately.  A conventional iterator cannot
    /// borrow from the map here: it would have to hold a shard's `RwLockReadGuard` and hand out
    /// references into it, which Rust cannot express without a self-referential struct and
    /// `unsafe`.  The alternative of yielding clones was measured at ~6.5x the cost of this on a
    /// 3300 entry map of 344 byte `StackStats` values, nearly all of it memcpy that the caller
    /// throws away after reading two fields, so that iterator was removed rather than left around
    /// as the ergonomic-looking slow path.
    ///
    /// Shards are locked one at a time, so this is not an atomic snapshot; see the type-level
    /// documentation.
    ///
    /// `f` runs while the entry's shard is read-locked, so it must neither allocate nor touch
    /// another profiler map.  Keep it to reading fields out of the value.
    ///
    /// **Nothing here allocates while a shard lock is held, and that is load-bearing.** Growing the
    /// output vector calls back into the profiler's own `realloc`, and a nested `realloc` that
    /// reaches the maps will block on a lock this very thread already holds.  That self-deadlock was
    /// observed in practice, with the profiler's re-entrancy guard defeated by an unrelated bug, and
    /// the captured backtrace pointed straight at a `Vec` growing inside this scan.  Relying on the
    /// guard alone means any future hole in it turns into a process-wide hang, so instead every
    /// allocation happens with no lock held: reserve for a shard, then fill within that reservation,
    /// and if the shard grew in between, reserve again and retry it.
    pub fn map_to_vec<T>(&self, mut f: impl FnMut(u64, &V) -> T) -> Vec<T> {
        let mut out = Vec::with_capacity(self.len());
        for shard in self.shards.iter() {
            loop {
                // Reserve with no lock held.  The slack makes a retry loop against a shard that is
                // actively growing terminate rather than chase it one entry at a time.
                let wanted = shard.read().unwrap_or_else(|e| e.into_inner()).len();
                out.reserve(wanted + wanted / 2 + 8);
                let spare = out.capacity() - out.len();

                let guard = shard.read().unwrap_or_else(|e| e.into_inner());
                if guard.len() > spare {
                    continue;
                }
                for (key, value) in guard.iter() {
                    // Cannot reallocate: `spare` was checked against this shard's length above
                    out.push(f(*key, value));
                }
                break;
            }
        }
        out
    }
}

impl<V: Clone> ShardedMap<V> {
    #[inline]
    pub fn get_cloned(&self, key: u64) -> Option<V> {
        self.read(key).get(&key).cloned()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::*;

    #[test]
    fn insert_get_remove() {
        let map = ShardedMap::with_capacity(100);
        assert_eq!(map.len(), 0);
        assert!(!map.contains_key(42));

        map.insert(42, 1u64);
        assert!(map.contains_key(42));
        assert_eq!(map.get_cloned(42), Some(1));
        assert_eq!(map.len(), 1);

        assert_eq!(map.remove(42), Some(1));
        assert_eq!(map.remove(42), None);
        assert_eq!(map.len(), 0);
    }

    #[test]
    fn update_or_insert_with_creates_then_updates() {
        let map = ShardedMap::with_capacity(10);

        map.update_or_insert_with(7, |v| *v += 100, || 1u64);
        assert_eq!(map.get_cloned(7), Some(1));

        map.update_or_insert_with(7, |v| *v += 100, || 1u64);
        assert_eq!(map.get_cloned(7), Some(101));
    }

    #[test]
    fn update_only_touches_existing_keys() {
        let map = ShardedMap::with_capacity(10);

        map.update(7, |v: &mut u64| *v += 1);
        assert_eq!(map.get_cloned(7), None);

        map.insert(7, 5u64);
        map.update(7, |v| *v += 1);
        assert_eq!(map.get_cloned(7), Some(6));
    }

    #[test]
    fn iteration_and_clear_cover_all_shards() {
        let map = ShardedMap::with_capacity(1000);
        for i in 0..1000u64 {
            map.insert(i, i * 2);
        }
        assert_eq!(map.len(), 1000);

        let entries = map.map_to_vec(|k, v| (k, *v));
        assert_eq!(entries.len(), 1000);
        for (k, v) in &entries {
            assert_eq!(*v, k * 2);
        }
        let sum: u64 = entries.iter().map(|(_, v)| v).sum();
        assert_eq!(sum, (0..1000u64).map(|i| i * 2).sum::<u64>());

        map.clear();
        assert_eq!(map.len(), 0);
        assert_eq!(map.map_to_vec(|k, _| k).len(), 0);
    }

    /// Every key must land in exactly one shard, and keys that are close together (as heap
    /// addresses are) must not all pile into the same one.
    #[test]
    fn adjacent_keys_spread_across_shards() {
        let map = ShardedMap::<u64>::with_capacity(0);
        let num_shards = map.shard_mask + 1;

        // Pointer-like keys: 16-byte aligned and consecutive
        let mut occupied = vec![0usize; num_shards];
        for i in 0..(num_shards * 100) {
            occupied[map.shard_of(0x7f00_0000_0000 + (i as u64) * 16)] += 1;
        }

        assert!(occupied.iter().all(|&c| c > 0), "some shard got no keys");
        // Perfectly even would be 100 per shard; allow a wide band, we only care that it is not
        // pathologically lumpy
        assert!(occupied.iter().all(|&c| c > 40 && c < 200), "{occupied:?}");
    }

    #[test]
    fn concurrent_inserts_from_many_threads() {
        let map = Arc::new(ShardedMap::with_capacity(100));
        let handles: Vec<_> = (0..8u64)
            .map(|t| {
                let map = map.clone();
                thread::spawn(move || {
                    for i in 0..1000 {
                        map.insert(t * 1000 + i, i);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(map.len(), 8000);
    }
}
