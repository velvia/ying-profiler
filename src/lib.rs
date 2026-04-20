//! Ying is a native Rust sampling memory profiler which tracks retained memory and allocations.  It is designed for
//! production usage in asynchronous Rust programs.  Ying(鷹) is Chinese for eagle.  🦅🦅🦅
//!
//! I started this experiment because existing solutions I looked at were either consuming too much resources, or wrote
//! profiling files that were too large or too cumbersome to consume, or were very bad at producing useful stack traces
//! especially for Rust async programs, or did not support tracking retained memory in its profiling.
//!
//! Features:
//! * Sampling profiler, so it uses little enough resources to be useful in production
//! * Track retained memory, including reallocs, as well as total allocations
//! * Targets async Rust programs (especially Tokio apps that use tracing for instrumentation), so you know the stack
//!   traces will be useful
//!   - Specific support for tracing spans and finding allocations by span
//!   - Removes extra `::poll::` lines in the stack trace for clarity
//! * Support for detecting leaks or large amounts of allocated memory that has not been freed
//!   - Tracks realloc() calls as single long-lived allocation
//! * Automatic and easy flamegraph generation
//! * Allocation lifetime/length histogram
//! * Track span information (need feature profile_spans) in stacks
//! * Get top stack traces by total allocation
//! * Get top traces by retained allocation
//! * `ProfilerRunner` -- utility to spin up thread to dump out reports and optionally flamegraphs every N minutes when
//!   total memory usage changes significantly
//! * Catch and prevent single allocations greater than say 64GB, dump out giant allocation stack trace
//!
//! To see an example which uses Ying and dumps out top stack traces by allocations:
//!
//! `cargo run --profile bench --features profile-spans --example ying_example`
//!
//! ## How to use
//!
//! ```rust
//! use ying_profiler::YingProfiler;
//!
//! #[global_allocator]
//! static YING_ALLOC: YingProfiler = YingProfiler::default();
//! ```
//!
//! The above sets Ying as the global allocator but does not dump out any profiles or stats.  Underneath Ying collects
//! stats and defers to the original System global allocator.
//!
//! To dump out stats, one can use methods in `YingProfiler`, but the easiest way is to use `ProfilerRunner`, which
//! starts up a background thread, checks memory use stats, and dumps out reports if the change in memory usage exceeds
//! some threshold.  The default checks every 5 minutes and dumps out a report if the retained memory changes by more
//! than 10%.  Options include a path to write out the text reports to, and if inlined stack frames should be expanded
//! in the reports.
//!
//! ## Why a new memory profiler?
//!
//! Rust as an ecosystem is lacking in good memory profiling tools.  [Bytehound](https://github.com/koute/bytehound)
//! is quite good but has a large CPU impact and writes out huge profiling files as it measures every allocation.
//! Jemalloc/Jeprof does sampling, so it's great for production use, but its output is difficult to interpret, and
//! it does not track retained memory, which is quite critical for debugging memory issues in production.
//! Both of the above tools are written with generic C/C++ malloc/preload ABI in mind, so the backtraces
//! that one gets from their use are really limited, especially for profiling Rust binaries built for
//! an optimized/release target, and especially async code.  The output also often has trouble with mangled symbols.
//!
//! If we use the backtrace crate and analyze release/bench backtraces in detail, we can see why that is.
//! The Rust compiler does a good job of inlining function calls - even ones across async/await boundaries -
//! in release code.  Thus, for a single instruction pointer (IP) in the stack trace, it might correspond to
//! many different places in the code.  This is from `examples/ying_example.rs`:
//!
//! ```bash
//! Some(ying_example::insert_one::{{closure}}::h7eddb5f8ebb3289b)
//!  > Some(<core::future::from_generator::GenFuture<T> as core::future::future::Future>::poll::h7a53098577c44da0)
//!  > Some(ying_example::cache_update_loop::{{closure}}::h38556c7e7ae06bfa)
//!  > Some(<core::future::from_generator::GenFuture<T> as core::future::future::Future>::poll::hd319a0f603a1d426)
//!  > Some(ying_example::main::{{closure}}::h33aa63760e836e2f)
//!  > Some(<core::future::from_generator::GenFuture<T> as core::future::future::Future>::poll::hb2fd3cb904946c24)
//! ```
//!
//! A generic tool which just examines the IP and tries to figure out a single symbol would miss out on all of the
//! inlined symbols.  Some tools can expand on symbols, but the results still aren't very good.
//!
//! ## Tracing support
//!
//! To add support for memory profiling of [tracing Spans](https://docs.rs/tracing/0.1.36/tracing/struct.Span.html),
//! enable the `profile-spans` feature of this crate.  Span information will be recorded.
//! NOTE: This feature is experimental and does not yet yield useful information.  It also causes a panic when
//! used with `tracing_subscriber` due to a problem with `current_span()` allocating and potentially causing a
//! RefCell `borrow()` to fail.
//!
use std::alloc::{GlobalAlloc, Layout, System};
use std::fmt::Write;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed, Ordering::SeqCst};

use backtrace::Backtrace;
use coarsetime::Clock;
use dashmap::DashMap;
use once_cell::sync::OnceCell;

pub mod callstack;
pub mod histogram;
pub mod utils;
use callstack::{FriendlySymbol, StackStats, StdCallstack};

/// The number of frames at the top of the stack to skip.  Most of these have to do with
/// backtrace and this profiler infrastructure.  This number needs to be adjusted
/// depending on the implementation.
/// For release/bench builds with debug = 1 / strip = none, this should be 4.
/// For debug builds, this is about 9.
const TOP_FRAMES_TO_SKIP: usize = 3;

const DEFAULT_GIANT_ALLOC_LIMIT: usize = 64 * 1024 * 1024 * 1024;

// A map for caching symbols in backtraces so we can mostly store u64's
type SymbolMap = DashMap<u64, Vec<FriendlySymbol>>;

/// Ying is a memory profiling Allocator wrapper.
/// Ying is the Chinese word for an eagle.
pub struct YingProfiler {
    /// Allocation sampling ratio.  Eg: 500 means 1 in 500 allocations are sampled.
    sampling_ratio: u32,
    /// Prevent and dump stack trace for giant single allocations beyond a certain size
    single_alloc_limit: usize,
    /// Global thread local state cache
    tl_cache: YingLocalCache,
    /// Statistics... lazily initialized later
    state: OnceCell<YingState>,
}

static TOTAL_RETAINED: AtomicUsize = AtomicUsize::new(0);
static PROFILED_ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static PROFILED_RETAINED: AtomicUsize = AtomicUsize::new(0);

impl YingProfiler {
    /// sampling_ratio: number of allocations for every sampled allocation
    pub const fn new(sampling_ratio: u32, single_alloc_limit: usize) -> Self {
        Self {
            sampling_ratio,
            single_alloc_limit,
            tl_cache: YingLocalCache::new(),
            state: OnceCell::new(),
        }
    }

    pub const fn default() -> Self {
        Self {
            sampling_ratio: 500,
            single_alloc_limit: DEFAULT_GIANT_ALLOC_LIMIT,
            tl_cache: YingLocalCache::new(),
            state: OnceCell::new(),
        }
    }

    /// Total outstanding retained bytes (not just sampled but all allocations)
    #[inline]
    pub fn total_retained_bytes() -> usize {
        TOTAL_RETAINED.load(Relaxed)
    }

    /// Total bytes allocated for profiled allocations
    #[inline]
    pub fn profiled_bytes_allocated() -> usize {
        PROFILED_ALLOCATED.load(Relaxed)
    }

    /// Profiled bytes retained - retained memory usage amongst profiled allocations
    #[inline]
    pub fn profiled_bytes_retained() -> usize {
        PROFILED_RETAINED.load(Relaxed)
    }

    #[inline]
    pub fn symbol_map_size(&self) -> usize {
        self.get_state().symbol_map.len()
    }

    /// Number of entries for outstanding sampled allocations map
    #[inline]
    pub fn num_outstanding_allocs(&self) -> usize {
        self.get_state().outstanding_allocs.len()
    }

    /// Get the top k stack traces by total profiled bytes allocated, in descending order.
    /// Note that "profiled bytes" refers to the bytes allocated during sampling by this profiler.
    pub fn top_k_stacks_by_allocated(&self, k: usize) -> Vec<StackStats> {
        self.lock_out_profiler(|| {
            let stacks_by_alloc = self.stack_list_allocated_bytes_desc();
            stacks_by_alloc
                .iter()
                .take(k)
                .filter_map(|&(stack_hash, _bytes_allocated)| {
                    self.get_stats_for_stack_hash(stack_hash)
                })
                .collect()
        })
    }

    /// Get the top k stack traces by retained sampled memory, in descending order.
    pub fn top_k_stacks_by_retained(&self, k: usize) -> Vec<StackStats> {
        self.lock_out_profiler(|| {
            let stacks_by_retained = self.stack_list_retained_bytes_desc();
            stacks_by_retained
                .iter()
                .take(k)
                .filter_map(|&(stack_hash, _bytes_retained)| {
                    self.get_stats_for_stack_hash(stack_hash)
                })
                .collect()
        })
    }

    /// Returns a list of stack IDs (stack_hash, bytes_allocated) in order from highest
    /// number of bytes allocated to lowest
    fn stack_list_allocated_bytes_desc(&self) -> Vec<(u64, u64)> {
        let mut items = Vec::new();
        // TODO: filter away entries with minimal allocations, say <1% or some threshold
        for entry in &self.get_state().stack_stats {
            items.push((*entry.key(), entry.value().allocated_bytes));
        }
        items.sort_unstable_by(|a, b| b.1.cmp(&a.1));
        items
    }

    /// Returns a list of stack IDs (stack_hash, bytes_retained) in order from highest
    /// number of bytes retained to lowest
    fn stack_list_retained_bytes_desc(&self) -> Vec<(u64, u64)> {
        let mut items = Vec::new();
        // TODO: filter away entries with minimal retained allocations, say <1% or some threshold
        for entry in &self.get_state().stack_stats {
            items.push((*entry.key(), entry.value().retained_profiled_bytes()));
        }
        items.sort_unstable_by(|a, b| b.1.cmp(&a.1));
        items
    }

    pub fn reset_state_for_testing_only(&self) {
        let state = self.get_state();
        state.stack_stats.clear();
        state.outstanding_allocs.clear();
    }

    pub fn testing_only_guarantee_next_sample(&self) {
        self.tl_cache
            .get_thread_local()
            .test_only_reset_sampling_counter()
    }

    #[inline]
    fn check_and_deny_giant_allocations(&self, ptr: *mut u8, layout: Layout) -> *mut u8 {
        // Sorry there is an edge case where this check cannot happen if YING is not initialized
        if layout.size() >= self.single_alloc_limit && self.state.get().is_some() {
            // Prevent allocation sampling while we are telling the world who did this
            self.lock_out_profiler(|| {
                println!(
                    "WARNING: Huge memory allocation of {} bytes denied by Ying profiler",
                    layout.size()
                );

                let mut bt = Backtrace::new_unresolved();

                // 2. Create a Callstack, check if there is a similar stack
                let stack = StdCallstack::from_backtrace_unresolved(&bt);
                let state = self.get_state();
                stack.populate_symbol_map(&mut bt, &state.symbol_map);
                println!(
                    "Stack trace:\n{}",
                    stack.with_symbols_and_filename(&state.symbol_map, true)
                );
            });
            std::ptr::null_mut::<u8>()
        } else {
            ptr
        }
    }

    #[inline]
    fn get_state(&self) -> &YingState {
        // We need to lock out the profiler here, to ensure no tracking of allocations or messes
        self.lock_out_profiler(|| self.state.get_or_init(YingState::new))
    }

    #[inline]
    fn get_stats_for_stack_hash(&self, stack_hash: u64) -> Option<StackStats> {
        self.get_state()
            .stack_stats
            .get(&stack_hash)
            .map(|r| r.value().clone())
    }

    /// Locks the profiler flag so that allocations are not profiled.
    /// This is for non-profiler code such as debug prints that has to access the Dashmap or state
    /// and could potentially cause deadlock problems with Dashmap for example.
    #[inline]
    fn lock_out_profiler<R>(&self, func: impl FnOnce() -> R) -> R {
        let tl_state = self.tl_cache.get_thread_local();
        tl_state.set_allocator_lock();
        let return_val = func();
        tl_state.release_allocator_lock();
        return_val
    }
}

// Private state.  We can't put this in the main YingProfiler struct as that one has to be const static
struct YingState {
    symbol_map: SymbolMap,
    // Main map of stack hash to StackStats
    stack_stats: DashMap<u64, StackStats>,
    // Map of outstanding sampled allocations.  Used to figure out amount of outstanding allocations and
    // statistics about how long lived outstanding allocations are.
    // (*ptr as u64 -> (stack hash, start_timestamp_epoch_millis))
    outstanding_allocs: DashMap<u64, (u64, u64)>,
}

impl YingState {
    pub fn new() -> Self {
        let symbol_map = SymbolMap::with_capacity(1000);
        let stack_stats = DashMap::with_capacity(1000);
        let outstanding_allocs = DashMap::with_capacity(5000);
        Self {
            symbol_map,
            stack_stats,
            outstanding_allocs,
        }
    }
}

/// Should be bigger than max number of unique CPUs for each process.  TODO: size based on n CPUs
const YING_CACHE_SIZE: usize = 1024;

// We can't use ThreadLocals in a GlobalAllocator, it's forbidden by the Rust runtime, and also because TLS
// may need to allocate to be set up.  Instead, we create a constant-sized "cache" of YingLocalCache state,
// hashed and indexed by thread ID.  This means we can quickly get at the re-entrant lock safely with no TLS,
// and relatively quickly.
// (We don't really need a global lock now, do we?)
// The general pattern of a thread cache is taken from https://www.brochweb.com/blog/post/how-to-create-a-custom-memory-allocator-in-rust/
struct YingLocalCache {
    local_states: [YingThreadLocal; YING_CACHE_SIZE],
}

impl YingLocalCache {
    const fn new() -> Self {
        Self {
            local_states: [YingThreadLocal::new(); YING_CACHE_SIZE],
        }
    }

    /// Returns the [YingThreadLocal] for the current thread.
    /// Note that this returns a mut ref, even though this is &self.
    #[allow(clippy::mut_from_ref)]
    #[inline]
    fn get_thread_local(&self) -> &mut YingThreadLocal {
        let r = &self.local_states[hash_usize(thread_id()) % YING_CACHE_SIZE];
        // # Safety
        // We can use unsafe here to give an exclusive/mutable reference because we have verified through
        // caching the thread ID that the returned YingThreadLocal should indeed be accessible only to that thread,
        // TODO: think about the case where thread ID exceeds 1024, could we get a scenario where different thread IDs
        // belonging to different CPU numbers hash to the same bucket?
        #[allow(mutable_transmutes)]
        unsafe {
            std::mem::transmute(r)
        }
    }
}

#[inline]
fn hash_usize(input: usize) -> usize {
    let mut output = input as u64;
    output ^= output >> 33;
    output = output.wrapping_mul(0xff51afd7ed558ccd);
    output ^= output >> 33;
    output = output.wrapping_mul(0xc4ceb9fe1a85ec53);
    output ^= output >> 33;
    output as usize
}

/// A struct to provide a better API around the lock out profiler flag/re-entrancy plus sampling
/// This is meant to be used ONLY in a thread-local and is definitely not multi-thread safe.
#[derive(Copy, Clone, PartialEq, Debug)]
struct YingThreadLocal {
    // Counts up for every time we enter a no-allocator critical section (ie where we have to touch
    // allocator state or cause an allocation within profiling-related code and don't want sampling
    // of re-entrant allocations done).  Nonzero prevents allocator from sampling.
    alloc_lock: u32,
    sample_count: u32,
}

impl YingThreadLocal {
    const fn new() -> Self {
        Self {
            alloc_lock: 0,
            sample_count: 0,
        }
    }

    #[inline]
    fn is_allocator_locked(&self) -> bool {
        self.alloc_lock > 0
    }

    #[inline]
    fn set_allocator_lock(&mut self) {
        self.alloc_lock = self.alloc_lock.saturating_add(1);
    }

    #[inline]
    fn release_allocator_lock(&mut self) {
        self.alloc_lock = self.alloc_lock.saturating_sub(1);
    }

    /// Obtains the counter, checks for sampling ratio, and updates counter in one go
    #[inline]
    fn should_sample(&mut self, ratio: u32) -> bool {
        self.sample_count += 1; // update counter for next sampling
        self.sample_count % ratio == 0
    }

    // Resets counter to 0 to guarantee next call to alloc() will sample.  TESTING ONLY
    #[inline]
    fn test_only_reset_sampling_counter(&mut self) {
        self.sample_count = 0;
    }
}

#[cfg(unix)]
pub(crate) fn thread_id() -> usize {
    unsafe { libc::pthread_self() as usize }
}

#[cfg(windows)]
pub(crate) fn thread_id() -> usize {
    unsafe { libc::GetCurrentThreadId() as usize }
}

unsafe impl GlobalAlloc for YingProfiler {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // NOTE: the code between here and the state.0 = true must be re-entrant
        // and therefore not allocate, otherwise there will be an infinite loop.
        let alloc_ptr = self.check_and_deny_giant_allocations(System.alloc(layout), layout);
        if !alloc_ptr.is_null() {
            TOTAL_RETAINED.fetch_add(layout.size(), SeqCst);

            // Now, sample allocation - if it falls below threshold, then profile
            // Also, we set a ThreadLocal to avoid re-entry: ie the code below might allocate,
            // and we avoid profiling if we are already in the loop below.  Avoids cycles.
            let tl_state = self.tl_cache.get_thread_local();
            if !tl_state.is_allocator_locked() && tl_state.should_sample(self.sampling_ratio) {
                tl_state.set_allocator_lock();

                PROFILED_ALLOCATED.fetch_add(layout.size(), SeqCst);
                PROFILED_RETAINED.fetch_add(layout.size(), SeqCst);

                // -- Beginning of section that may allocate
                // 1. Get unresolved backtrace for speed
                let mut bt = Backtrace::new_unresolved();

                // 2. Create a Callstack, check if there is a similar stack
                let stack = StdCallstack::from_backtrace_unresolved(&bt);
                let stack_hash = stack.compute_hash();
                self.get_state()
                    .stack_stats
                    .entry(stack_hash)
                    .and_modify(|stats| {
                        // 4. Update stats
                        stats.num_allocations += 1;
                        stats.allocated_bytes += layout.size() as u64;
                    })
                    .or_insert_with(|| {
                        // 3. Resolve symbols if needed (new stack entry)
                        stack.populate_symbol_map(&mut bt, &self.get_state().symbol_map);
                        StackStats::new(stack, Some(layout.size() as u64))
                    });

                // 4. Record allocation so we can track outstanding vs transient allocs
                self.get_state()
                    .outstanding_allocs
                    .entry(alloc_ptr as u64)
                    .or_insert_with(|| (stack_hash, Clock::recent_since_epoch().as_millis()));

                // -- End of core profiling section, no more allocations --
                tl_state.release_allocator_lock();
            }
        }
        alloc_ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
        TOTAL_RETAINED.fetch_sub(layout.size(), SeqCst);

        // Return immediately and skip rest of this if YING_STATE is not initialized.  It could cause
        // an infinite loop because during initialization of YING_STATE, dealloc() could be then called
        if self.state.get().is_none() {
            return;
        }

        // If the allocation was recorded in outstanding_allocs, then remove it and update stats
        // about number of bytes freed etc.  Do this with protection to guard against possible re-entry.
        let state = self.get_state();
        if state.outstanding_allocs.contains_key(&(ptr as u64)) {
            PROFILED_RETAINED.fetch_sub(layout.size(), SeqCst);
            let tl_state = self.tl_cache.get_thread_local();
            if !tl_state.is_allocator_locked() {
                tl_state.set_allocator_lock();

                // -- Beginning of section that may allocate
                if let Some((_, (stack_hash, alloc_ts))) =
                    state.outstanding_allocs.remove(&(ptr as u64))
                {
                    let alloc_time_ms = Clock::recent_since_epoch()
                        .as_millis()
                        .saturating_sub(alloc_ts);

                    // Update memory profiling freed bytes stats
                    state.stack_stats.entry(stack_hash).and_modify(|stats| {
                        stats.update_free_stats(layout.size() as u64, alloc_time_ms)
                    });
                }

                // -- End of core profiling section, no more allocations --
                tl_state.release_allocator_lock();
            }
        }
    }

    // We implement a custom realloc().  We must count reallocs as the same allocation, but need to do
    // the following: - update original allocated bytes (but not allocations); move outstanding_allocs
    // because the pointer moved, but preserve original starting timestamp.
    // The above also saves us cycles from having to call alloc() and dealloc() separately.
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let old_size = layout.size();
        // SAFETY: the caller must ensure that the `new_size` does not overflow.
        // `layout.align()` comes from a `Layout` and is thus guaranteed to be valid.
        let new_layout = Layout::from_size_align_unchecked(new_size, layout.align());
        // SAFETY: the caller must ensure that `new_layout` is greater than zero.
        let new_ptr = self.check_and_deny_giant_allocations(System.alloc(new_layout), new_layout);
        if !new_ptr.is_null() {
            // SAFETY: the previously allocated block cannot overlap the newly allocated block.
            // The safety contract for `dealloc` must be upheld by the caller.
            std::ptr::copy_nonoverlapping(ptr, new_ptr, std::cmp::min(old_size, new_size));
            System.dealloc(ptr, layout);

            // 1. Update global statistics
            if new_size > old_size {
                TOTAL_RETAINED.fetch_add(new_size - old_size, SeqCst);
            } else {
                TOTAL_RETAINED.fetch_sub(old_size - new_size, SeqCst);
            }

            // 2. IF the old pointer was in outstanding_allocs, move it and make a new entry,
            //    keeping the old starting timestamp.  Also update stack stats.
            //    But only if state is alredy initialized - otherwise any state initialization that
            //    results in a realloc() could cause this to infinite loop
            let state = self.get_state();
            if state.outstanding_allocs.contains_key(&(ptr as u64)) {
                if new_size > old_size {
                    PROFILED_RETAINED.fetch_add(new_size - old_size, SeqCst);
                } else {
                    PROFILED_RETAINED.fetch_sub(old_size - new_size, SeqCst);
                }

                let tl_state = self.tl_cache.get_thread_local();
                if !tl_state.is_allocator_locked() {
                    tl_state.set_allocator_lock();

                    // -- Beginning of section that may allocate
                    if let Some((_, (stack_hash, alloc_ts))) =
                        state.outstanding_allocs.remove(&(ptr as u64))
                    {
                        state
                            .outstanding_allocs
                            .insert(new_ptr as u64, (stack_hash, alloc_ts));

                        // Update memory profiling freed bytes stats
                        state.stack_stats.entry(stack_hash).and_modify(|stats| {
                            if new_size > old_size {
                                stats.allocated_bytes += (new_size - old_size) as u64;
                            } else {
                                stats.allocated_bytes -= (old_size - new_size) as u64;
                            }
                            // Don't change number of allocations or frees
                        });
                    }

                    // -- End of core profiling section, no more allocations --
                    tl_state.release_allocator_lock();
                }
            }
        }
        new_ptr
    }
}
