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
//! Set Ying as the global allocator, then turn on reporting with one line:
//!
//! ```no_run
//! use ying_profiler::YingProfiler;
//!
//! #[global_allocator]
//! static YING_ALLOC: YingProfiler = YingProfiler::default();
//!
//! fn main() {
//!     YING_ALLOC.start_profiling();
//!     // ... the rest of your program
//! }
//! ```
//!
//! There are three layers here, and it is worth knowing which one you are using.
//!
//! ### 1. The allocator: collects, never reports
//!
//! The `#[global_allocator]` declaration is what makes Ying sample allocations at all.  On its own
//! it is complete and valid: Ying accumulates stats in memory and defers to the System allocator,
//! but writes nothing anywhere.  Nothing is scheduled and no thread is spawned.
//!
//! ### 2. [`ProfilerRunner`]: the primitive that decides how stats get reported
//!
//! [`ProfilerRunner`] is the piece to reach for whenever the defaults do not fit.  It owns every
//! decision about turning collected stats into output — how often to look, how much of a change is
//! worth reporting, what to measure, where output goes, and in what form:
//!
//! | Setting | Meaning |
//! |---|---|
//! | `check_interval_secs` | how often the background thread wakes up to compare memory use |
//! | `report_pct_change_trigger` | how much retained memory must move before a report is written |
//! | `reporting_path` | directory for reports and flamegraphs; created if missing |
//! | `measure_allocated_not_retained` | rank stacks by total allocated bytes instead of retained |
//! | `gen_flamegraphs` | also write an SVG flamegraph alongside each text report |
//! | `expand_frames` | expand inlined symbols within each stack frame in reports |
//!
//! Build one with [`ProfilerRunnerBuilder`](utils::ProfilerRunnerBuilder), which defaults
//! anything you leave out, then hand it
//! the allocator static to start its thread:
//!
//! ```no_run
//! use ying_profiler::{utils::ProfilerRunnerBuilder, YingProfiler};
//!
//! #[global_allocator]
//! static YING_ALLOC: YingProfiler = YingProfiler::default();
//!
//! fn main() {
//!     let runner = ProfilerRunnerBuilder::default()
//!         .check_interval_secs(60usize)
//!         .report_pct_change_trigger(5usize)
//!         .reporting_path("/var/log/ying")
//!         .gen_flamegraphs(true)
//!         .build()
//!         .unwrap();
//!     runner.spawn(&YING_ALLOC);
//! }
//! ```
//!
//! [`YingProfiler::start_profiling`] is not a separate mechanism: it builds exactly one of these
//! with a specific set of values (5 minute interval, 10% trigger, flamegraphs on, retained memory,
//! output to `ying-profiles`) and spawns it.  Anything it can do, a [`ProfilerRunner`] you build
//! yourself can do too.
//!
//! ### 3. Reading stats directly
//!
//! You do not need a runner at all if you would rather decide when to look.  [`YingProfiler`]
//! exposes the stats as data — [`YingProfiler::top_k_stacks_by_retained`],
//! [`YingProfiler::top_k_stacks_by_allocated`], [`YingProfiler::total_retained_bytes`] — and
//! [`utils::gen_flamegraph`] writes a one-off flamegraph on demand.  This is the route to take when
//! reports should be triggered by your own signals, such as an HTTP endpoint or a health check
//! noticing memory growth.
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
use std::cell::Cell;
use std::cmp::min;
use std::env::var;
use std::fmt::Write;
use std::mem::needs_drop;
use std::ptr::{copy_nonoverlapping, null_mut};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed, Ordering::SeqCst};

use backtrace::Backtrace;
use coarsetime::Clock;
use once_cell::sync::OnceCell;

pub mod callstack;
pub mod histogram;
mod sharded_map;
pub mod utils;
use callstack::{FriendlySymbol, StackStats, StdCallstack};
use sharded_map::ShardedMap;
use utils::{
    ProfilerRunner, DEFAULT_CHECK_INTERVAL_SECS, DEFAULT_PCT_CHANGE_TRIGGER, DEFAULT_REPORTING_PATH,
};

/// The number of frames at the top of the stack to skip.  Most of these have to do with
/// backtrace and this profiler infrastructure.  This number needs to be adjusted
/// depending on the implementation.
/// For release/bench builds with debug = 1 / strip = none, this should be 4.
/// For debug builds, this is about 9.
const TOP_FRAMES_TO_SKIP: usize = 3;

const DEFAULT_GIANT_ALLOC_LIMIT: usize = 64 * 1024 * 1024 * 1024;

/// Testing knob: overrides the initial capacity of every profiler map.  Not part of the public
/// API, and read exactly once when profiler state is first initialized.
const MAP_CAPACITY_ENV_VAR: &str = "YING_TEST_MAP_CAPACITY";

// A map for caching symbols in backtraces so we can mostly store u64's
pub(crate) type SymbolMap = ShardedMap<Vec<FriendlySymbol>>;

/// Ying is a memory profiling Allocator wrapper.
/// Ying is the Chinese word for an eagle.
pub struct YingProfiler {
    /// Allocation sampling ratio.  Eg: 500 means 1 in 500 allocations are sampled.
    sampling_ratio: u32,
    /// Prevent and dump stack trace for giant single allocations beyond a certain size
    single_alloc_limit: usize,
    /// Global thread local state cache
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
            state: OnceCell::new(),
        }
    }

    pub const fn default() -> Self {
        Self {
            sampling_ratio: 500,
            single_alloc_limit: DEFAULT_GIANT_ALLOC_LIMIT,
            state: OnceCell::new(),
        }
    }

    /// Starts periodic memory reporting on a background thread and returns the [`ProfilerRunner`]
    /// that was spawned.  This is the one-line way to turn Ying on:
    ///
    /// ```no_run
    ///     use ying_profiler::YingProfiler;
    ///
    ///     #[global_allocator]
    ///     static YING_ALLOC: YingProfiler = YingProfiler::default();
    ///
    ///     fn main() {
    ///         YING_ALLOC.start_profiling();
    ///     }
    /// ```
    ///
    /// This is a convenience only, and adds no capability of its own: it builds a
    /// [`ProfilerRunner`] with a fixed set of values and calls [`ProfilerRunner::spawn`].  Those
    /// values are a 5 minute [`DEFAULT_CHECK_INTERVAL_SECS`] check interval, a 10%
    /// [`DEFAULT_PCT_CHANGE_TRIGGER`] change before a report is written, output to
    /// [`DEFAULT_REPORTING_PATH`] (created if it does not exist), flamegraphs on, retained rather
    /// than allocated memory, and inlined frames left unexpanded.
    ///
    /// To change any of those, build the [`ProfilerRunner`] yourself with
    /// [`ProfilerRunnerBuilder`](utils::ProfilerRunnerBuilder) and call
    /// [`spawn`](ProfilerRunner::spawn) on it instead.  The returned runner records the settings in
    /// use, but the reporting thread has already been started with them; changing the returned
    /// value has no effect.
    pub fn start_profiling(&'static self) -> ProfilerRunner {
        let runner = ProfilerRunner::new(
            DEFAULT_CHECK_INTERVAL_SECS,
            DEFAULT_PCT_CHANGE_TRIGGER,
            DEFAULT_REPORTING_PATH,
            false,
            true,
            false,
        );
        runner.spawn(self);
        runner
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
        self.lock_out_profiler(|| self.get_state().symbol_map.len())
    }

    /// Number of entries for outstanding sampled allocations map
    #[inline]
    pub fn num_outstanding_allocs(&self) -> usize {
        self.lock_out_profiler(|| self.get_state().outstanding_allocs.len())
    }

    /// Number of distinct stack traces that have been sampled
    #[inline]
    pub fn num_stack_traces(&self) -> usize {
        self.lock_out_profiler(|| self.get_state().stack_stats.len())
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
        // TODO: filter away entries with minimal allocations, say <1% or some threshold
        let mut items = self
            .get_state()
            .stack_stats
            .map_to_vec(|stack_hash, stats| (stack_hash, stats.allocated_bytes));
        items.sort_unstable_by(|a, b| b.1.cmp(&a.1));
        items
    }

    /// Returns a list of stack IDs (stack_hash, bytes_retained) in order from highest
    /// number of bytes retained to lowest
    fn stack_list_retained_bytes_desc(&self) -> Vec<(u64, u64)> {
        // TODO: filter away entries with minimal retained allocations, say <1% or some threshold
        let mut items = self
            .get_state()
            .stack_stats
            .map_to_vec(|stack_hash, stats| (stack_hash, stats.retained_profiled_bytes()));
        items.sort_unstable_by(|a, b| b.1.cmp(&a.1));
        items
    }

    pub fn reset_state_for_testing_only(&self) {
        self.lock_out_profiler(|| {
            let state = self.get_state();
            state.stack_stats.clear();
            state.outstanding_allocs.clear();
        })
    }

    pub fn testing_only_guarantee_next_sample(&self) {
        THREAD_STATE.with(YingThreadLocal::test_only_reset_sampling_counter)
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
            null_mut::<u8>()
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
        self.get_state().stack_stats.get_cloned(stack_hash)
    }

    /// Locks the profiler flag so that allocations are not profiled.
    /// This is for non-profiler code such as debug prints that has to access the Dashmap or state
    /// and could potentially cause deadlock problems with Dashmap for example.
    #[inline]
    fn lock_out_profiler<R>(&self, func: impl FnOnce() -> R) -> R {
        // Deliberately three short thread local accesses rather than running `func` inside a
        // `with`: `func` frequently calls back in here (get_state does), and keeping the borrow
        // scopes disjoint means nesting needs no reasoning about re-entrant `with`.
        THREAD_STATE.with(YingThreadLocal::set_allocator_lock);
        let return_val = func();
        exit_profiler();
        return_val
    }
}

// Private state.  We can't put this in the main YingProfiler struct as that one has to be const static
struct YingState {
    symbol_map: SymbolMap,
    // Main map of stack hash to StackStats
    stack_stats: ShardedMap<StackStats>,
    // Map of outstanding sampled allocations.  Used to figure out amount of outstanding allocations and
    // statistics about how long lived outstanding allocations are.
    // (*ptr as u64 -> (stack hash, start_timestamp_epoch_millis))
    outstanding_allocs: ShardedMap<(u64, u64)>,
}

impl YingState {
    pub fn new() -> Self {
        // Shrinking the maps makes them resize constantly, which is how the re-entrancy deadlocks
        // were shaken out; the stress tests set this so those paths get exercised on every run.
        let capacity_scale = var(MAP_CAPACITY_ENV_VAR)
            .ok()
            .and_then(|s| s.parse::<usize>().ok());
        let symbol_map = SymbolMap::with_capacity(capacity_scale.unwrap_or(1000));
        let stack_stats = ShardedMap::with_capacity(capacity_scale.unwrap_or(1000));
        let outstanding_allocs = ShardedMap::with_capacity(capacity_scale.unwrap_or(5000));
        Self {
            symbol_map,
            stack_stats,
            outstanding_allocs,
        }
    }
}

thread_local! {
    /// Per-thread profiler state, in a thread local rather than a slot in a fixed-size table.
    ///
    /// An earlier design hashed the thread id into a 1024 entry array and handed out `&mut` to the
    /// slot, on the reasoning that a `thread_local!` cannot be used from a `GlobalAlloc` because
    /// setting up TLS may allocate and re-enter the allocator.  That reasoning was sound for a lazily
    /// initialized thread local, but two threads whose ids collide then share one slot, which is both
    /// aliasing UB and a lost re-entrancy guard: a non-atomic increment of `alloc_lock` can be dropped,
    /// the guard falls to zero while a thread is still inside its critical section, and its next
    /// internal free re-enters the maps and blocks on a shard lock it already holds.  That deadlock was
    /// observed, and the captured backtrace showed exactly that shape.
    ///
    /// The `const {}` initializer plus a type that needs no `Drop` is what makes this safe: it skips
    /// lazy initialization and destructor registration, so no Rust allocation happens on access.  On
    /// ELF targets linked into an executable this compiles to a single thread-pointer-relative load.
    /// macOS still routes through a resolver call that allocates the thread's TLV block on first touch,
    /// but that allocation is libc `malloc` inside dyld, which `#[global_allocator]` does not
    /// interpose, so it cannot re-enter Ying.  Measured: 21,225 accesses from inside `alloc` and
    /// `dealloc` across 50 freshly spawned threads and their teardown, with zero re-entry.
    ///
    /// Keep both invariants if you touch this.  Adding a field that needs dropping, or dropping the
    /// `const {}`, reintroduces an allocating first access and with it the deadlock.
    static THREAD_STATE: YingThreadLocal = const { YingThreadLocal::new() };
}

/// Enforces the invariant that [`THREAD_STATE`] documents.  A field that needs dropping would make
/// the thread local register a destructor, which allocates on first access and re-enters the
/// allocator, so this is a compile error rather than a comment nobody reads.
const _: () = assert!(
    !needs_drop::<YingThreadLocal>(),
    "YingThreadLocal must not need Drop, or thread local setup will allocate inside the allocator"
);

/// A struct to provide a better API around the lock out profiler flag/re-entrancy plus sampling.
/// Interior mutability via [`Cell`] rather than `&mut`, so that no field needs `Drop` and the
/// thread local stays on its cheapest code path.
struct YingThreadLocal {
    // Counts up for every time we enter a no-allocator critical section (ie where we have to touch
    // allocator state or cause an allocation within profiling-related code and don't want sampling
    // of re-entrant allocations done).  Nonzero prevents allocator from sampling.
    alloc_lock: Cell<u32>,
    sample_count: Cell<u32>,
}

impl YingThreadLocal {
    const fn new() -> Self {
        Self {
            alloc_lock: Cell::new(0),
            sample_count: Cell::new(0),
        }
    }

    #[inline]
    fn is_allocator_locked(&self) -> bool {
        self.alloc_lock.get() > 0
    }

    #[inline]
    fn set_allocator_lock(&self) {
        self.alloc_lock.set(self.alloc_lock.get().saturating_add(1));
    }

    #[inline]
    fn release_allocator_lock(&self) {
        self.alloc_lock.set(self.alloc_lock.get().saturating_sub(1));
    }

    /// Obtains the counter, checks for sampling ratio, and updates counter in one go
    #[inline]
    fn should_sample(&self, ratio: u32) -> bool {
        let count = self.sample_count.get().wrapping_add(1);
        self.sample_count.set(count);
        count % ratio == 0
    }

    // Resets counter to 0 to guarantee next call to alloc() will sample.  TESTING ONLY
    #[inline]
    fn test_only_reset_sampling_counter(&self) {
        self.sample_count.set(0);
    }
}

/// Takes the re-entrancy guard if this thread is not already inside the profiler, reporting whether
/// the caller now owns it and must release it.
///
/// Deliberately one thread local access rather than several: this runs on every allocation, and the
/// common answer is "do not profile this one".
#[inline]
fn try_enter_profiler(sampling_ratio: Option<u32>) -> bool {
    THREAD_STATE.with(|tl| {
        if tl.is_allocator_locked() {
            return false;
        }
        // Only consume a sampling tick when we were not already locked out, matching the original
        // short-circuiting behaviour
        if let Some(ratio) = sampling_ratio {
            if !tl.should_sample(ratio) {
                return false;
            }
        }
        tl.set_allocator_lock();
        true
    })
}

#[inline]
fn exit_profiler() {
    THREAD_STATE.with(YingThreadLocal::release_allocator_lock)
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
            if try_enter_profiler(Some(self.sampling_ratio)) {
                PROFILED_ALLOCATED.fetch_add(layout.size(), SeqCst);
                PROFILED_RETAINED.fetch_add(layout.size(), SeqCst);

                // -- Beginning of section that may allocate
                // 1. Get unresolved backtrace for speed
                let mut bt = Backtrace::new_unresolved();

                // 2. Create a Callstack, check if there is a similar stack
                let stack = StdCallstack::from_backtrace_unresolved(&bt);
                let stack_hash = stack.compute_hash();
                let state = self.get_state();
                state.stack_stats.update_or_insert_with(
                    stack_hash,
                    |stats| {
                        // 4. Update stats
                        stats.num_allocations += 1;
                        stats.allocated_bytes += layout.size() as u64;
                    },
                    || {
                        // 3. Resolve symbols if needed (new stack entry).
                        // NOTE: this runs while the stack_stats shard is locked, but it only ever
                        // touches symbol_map, which is a separate map, so it cannot self-deadlock.
                        stack.populate_symbol_map(&mut bt, &state.symbol_map);
                        StackStats::new(stack, Some(layout.size() as u64))
                    },
                );

                // 4. Record allocation so we can track outstanding vs transient allocs
                state.outstanding_allocs.update_or_insert_with(
                    alloc_ptr as u64,
                    |_existing| {},
                    || (stack_hash, Clock::recent_since_epoch().as_millis()),
                );

                // -- End of core profiling section, no more allocations --
                exit_profiler();
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

        // IMPORTANT: the re-entrancy check has to happen before we touch any of the profiler maps.
        // The maps allocate and free internally (eg when a shard resizes), and those frees land back
        // here while that shard's write lock is held.  Touching the map first would then try to take
        // a read lock the same thread already holds exclusively, which self-deadlocks.
        if !try_enter_profiler(None) {
            return;
        }

        // -- Beginning of section that may allocate
        // If the allocation was recorded in outstanding_allocs, then remove it and update stats
        // about number of bytes freed etc.
        let state = self.get_state();
        if state.outstanding_allocs.contains_key(ptr as u64) {
            if let Some((stack_hash, alloc_ts)) = state.outstanding_allocs.remove(ptr as u64) {
                PROFILED_RETAINED.fetch_sub(layout.size(), SeqCst);
                let alloc_time_ms = Clock::recent_since_epoch()
                    .as_millis()
                    .saturating_sub(alloc_ts);

                // Update memory profiling freed bytes stats
                state.stack_stats.update(stack_hash, |stats| {
                    stats.update_free_stats(layout.size() as u64, alloc_time_ms)
                });
            }
        }

        // -- End of core profiling section, no more allocations --
        exit_profiler();
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
            copy_nonoverlapping(ptr, new_ptr, min(old_size, new_size));
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
            if self.state.get().is_none() {
                return new_ptr;
            }

            // As in dealloc(), the re-entrancy check must come before any map access, or a map's own
            // internal realloc re-enters here while holding that shard's write lock and deadlocks.
            if !try_enter_profiler(None) {
                return new_ptr;
            }

            // -- Beginning of section that may allocate
            let state = self.get_state();
            if state.outstanding_allocs.contains_key(ptr as u64) {
                if let Some((stack_hash, alloc_ts)) = state.outstanding_allocs.remove(ptr as u64) {
                    if new_size > old_size {
                        PROFILED_RETAINED.fetch_add(new_size - old_size, SeqCst);
                    } else {
                        PROFILED_RETAINED.fetch_sub(old_size - new_size, SeqCst);
                    }

                    state
                        .outstanding_allocs
                        .insert(new_ptr as u64, (stack_hash, alloc_ts));

                    // Update memory profiling freed bytes stats
                    state.stack_stats.update(stack_hash, |stats| {
                        if new_size > old_size {
                            stats.allocated_bytes += (new_size - old_size) as u64;
                        } else {
                            stats.allocated_bytes -= (old_size - new_size) as u64;
                        }
                        // Don't change number of allocations or frees
                    });
                }
            }

            // -- End of core profiling section, no more allocations --
            exit_profiler();
        }
        new_ptr
    }
}
