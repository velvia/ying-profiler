//! A memory profiling global allocator.
//! Experimental only right now.
//!
//! Goals:
//! - Use Rust backtrace to properly expose async call stack
//! - Sampled profiling for production efficiency
//! - Tie in to Tokio tracing spans for context richness
//!
//! ## Why a new memory profiler?
//!
//! Rust as an ecosystem is lacking in good memory profiling tools.  [Bytehound](https://github.com/koute/bytehound)
//! is quite good but has a large CPU impact and writes out huge profiling files as it measures every allocation.
//! Jemalloc/Jeprof does sampling, so it's great for production use, but its output is difficult to interpret.
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
use std::fmt::Write;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed, Ordering::SeqCst};

use backtrace::Backtrace;
use coarsetime::Clock;
use dashmap::DashMap;
use once_cell::sync::Lazy;

pub mod callstack;
pub mod utils;
use callstack::{FriendlySymbol, StdCallstack};

/// The number of frames at the top of the stack to skip.  Most of these have to do with
/// backtrace and this profiler infrastructure.  This number needs to be adjusted
/// depending on the implementation.
/// For release/bench builds with debug = 1 / strip = none, this should be 4.
/// For debug builds, this is about 9.
const TOP_FRAMES_TO_SKIP: usize = 4;

const DEFAULT_GIANT__ALLOC_LIMIT: usize = 64 * 1024 * 1024 * 1024;

// A map for caching symbols in backtraces so we can mostly store u64's
type SymbolMap = DashMap<u64, Vec<FriendlySymbol>>;

/// Ying is a memory profiling Allocator wrapper.
/// Ying is the Chinese word for an eagle.
pub struct YingProfiler {
    /// Allocation sampling ratio.  Eg: 500 means 1 in 500 allocations are sampled.
    sampling_ratio: u32,
    /// Prevent and dump stack trace for giant single allocations beyond a certain size
    single_alloc_limit: usize,
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
        }
    }

    pub const fn default() -> Self {
        Self {
            sampling_ratio: 500,
            single_alloc_limit: DEFAULT_GIANT__ALLOC_LIMIT,
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
    pub fn profiled_bytes_retained() -> usize {
        PROFILED_RETAINED.load(Relaxed)
    }

    #[inline]
    pub fn symbol_map_size() -> usize {
        YING_STATE.symbol_map.len()
    }

    /// Number of entries for outstanding sampled allocations map
    #[inline]
    pub fn num_outstanding_allocs() -> usize {
        YING_STATE.outstanding_allocs.len()
    }

    /// Get the top k stack traces by total profiled bytes allocated, in descending order.
    /// Note that "profiled bytes" refers to the bytes allocated during sampling by this profiler.
    pub fn top_k_stacks_by_allocated(k: usize) -> Vec<StackStats> {
        lock_out_profiler(|| {
            let stacks_by_alloc = stack_list_allocated_bytes_desc();
            stacks_by_alloc
                .iter()
                .take(k)
                .filter_map(|&(stack_hash, _bytes_allocated)| get_stats_for_stack_hash(stack_hash))
                .collect()
        })
    }

    /// Get the top k stack traces by retained sampled memory, in descending order.
    pub fn top_k_stacks_by_retained(k: usize) -> Vec<StackStats> {
        lock_out_profiler(|| {
            let stacks_by_retained = stack_list_retained_bytes_desc();
            stacks_by_retained
                .iter()
                .take(k)
                .filter_map(|&(stack_hash, _bytes_retained)| get_stats_for_stack_hash(stack_hash))
                .collect()
        })
    }

    fn check_and_deny_giant_allocations(&self, ptr: *mut u8, layout: Layout) -> *mut u8 {
        // Sorry there is an edge case where this check cannot happen if YING is not initialized
        if layout.size() >= self.single_alloc_limit && Lazy::get(&YING_STATE).is_some() {
            // Prevent allocation sampling while we are telling the world who did this
            lock_out_profiler(|| {
                println!(
                    "WARNING: Huge memory allocation of {} bytes denied by Ying profiler",
                    layout.size()
                );

                let mut bt = Backtrace::new_unresolved();

                // 2. Create a Callstack, check if there is a similar stack
                let stack = StdCallstack::from_backtrace_unresolved(&bt);
                stack.populate_symbol_map(&mut bt, &YING_STATE.symbol_map);
                println!(
                    "Stack trace:\n{}",
                    stack.with_symbols_and_filename(&YING_STATE.symbol_map)
                );
            });
            std::ptr::null_mut::<u8>()
        } else {
            ptr
        }
    }
}

/// Central struct collecting stats about each stack trace
#[derive(Debug, Clone)]
pub struct StackStats {
    stack: StdCallstack,
    pub allocated_bytes: u64,
    pub num_allocations: u64,
    pub freed_bytes: u64,
    pub num_frees: u64,
    #[cfg(feature = "profile-spans")]
    span: tracing::Span,
}

impl StackStats {
    // Constructor not public.  Only this crate should create new stats.
    fn new(stack: StdCallstack, initial_alloc_bytes: Option<u64>) -> Self {
        Self {
            stack,
            allocated_bytes: initial_alloc_bytes.unwrap_or(0),
            num_allocations: initial_alloc_bytes.map(|_| 1).unwrap_or(0),
            freed_bytes: 0,
            num_frees: 0,
            #[cfg(feature = "profile-spans")]
            span: tracing::Span::current(),
        }
    }

    /// The number of "retained" bytes as seen by this profiler from sampling
    pub fn retained_profiled_bytes(&self) -> u64 {
        // NOTE: saturating_sub here is really important, freed could be slightly bigger than allocated
        self.allocated_bytes.saturating_sub(self.freed_bytes)
    }

    /// Create a rich multi-line report of this StackStats
    /// * filename - include source filename in stack trace
    pub fn rich_report(&self, with_filenames: bool) -> String {
        let profiled_alloc_bytes = YingProfiler::profiled_bytes_allocated();
        let pct = (self.allocated_bytes as f64) * 100.0 / (profiled_alloc_bytes as f64);
        let mut report = format!(
            "{} profiled bytes allocated ({pct:.2}%) ({} allocations)\n",
            self.allocated_bytes, self.num_allocations
        );
        let retained = self.retained_profiled_bytes();
        let _ = writeln!(
            &mut report,
            "  {} profiled bytes retained  ({} frees)",
            retained, self.num_frees
        );
        let retained_pct_allocs = (retained as f64) * 100.0 / (self.allocated_bytes as f64);
        let retained_pct_all =
            (retained as f64) * 100.0 / YingProfiler::profiled_bytes_retained() as f64;
        let _ = writeln!(
            &mut report,
            "    ({retained_pct_all:.2}% of all retained profiled allocs) ({retained_pct_allocs:.2}% of allocated bytes)",
        );

        #[cfg(feature = "profile-spans")]
        if !self.span.is_disabled() {
            let _ = writeln!(&mut report, "\ttracing span id: {:?}", self.span.id());
        }

        // TODO: this won't be needed once we upgrade from dashmap to something which does atomic reads
        // Also try to make locking or accesses more fine grained
        lock_out_profiler(|| {
            let decorated_stack = if with_filenames {
                self.stack.with_symbols_and_filename(&YING_STATE.symbol_map)
            } else {
                self.stack.with_symbols(&YING_STATE.symbol_map)
            };
            let _ = writeln!(&mut report, "{}", decorated_stack);
        });
        report
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

// lazily initialized global state
static YING_STATE: Lazy<YingState> = Lazy::new(|| {
    // We need to disable the profiler in here as it could cause an endless loop otherwise trying to initialize
    PROFILER_TL.with(|tl_state| {
        tl_state.set_allocator_lock();

        let symbol_map = SymbolMap::with_capacity(1000);
        let stack_stats = DashMap::with_capacity(1000);
        let outstanding_allocs = DashMap::with_capacity(5000);
        let s = YingState {
            symbol_map,
            stack_stats,
            outstanding_allocs,
        };

        tl_state.release_allocator_lock();
        s
    })
});

fn get_stats_for_stack_hash(stack_hash: u64) -> Option<StackStats> {
    YING_STATE
        .stack_stats
        .get(&stack_hash)
        .map(|r| r.value().clone())
}

/// Returns a list of stack IDs (stack_hash, bytes_allocated) in order from highest
/// number of bytes allocated to lowest
fn stack_list_allocated_bytes_desc() -> Vec<(u64, u64)> {
    let mut items = Vec::new();
    // TODO: filter away entries with minimal allocations, say <1% or some threshold
    for entry in &YING_STATE.stack_stats {
        items.push((*entry.key(), entry.value().allocated_bytes));
    }
    items.sort_unstable_by(|a, b| b.1.cmp(&a.1));
    items
}

/// Returns a list of stack IDs (stack_hash, bytes_retained) in order from highest
/// number of bytes retained to lowest
fn stack_list_retained_bytes_desc() -> Vec<(u64, u64)> {
    let mut items = Vec::new();
    // TODO: filter away entries with minimal retained allocations, say <1% or some threshold
    for entry in &YING_STATE.stack_stats {
        items.push((*entry.key(), entry.value().retained_profiled_bytes()));
    }
    items.sort_unstable_by(|a, b| b.1.cmp(&a.1));
    items
}

pub fn reset_state_for_testing_only() {
    YING_STATE.stack_stats.clear();
    YING_STATE.outstanding_allocs.clear();
}

// NOTE: The creation of state in this TL must NOT allocate. Otherwise it will cause
// the profiler code to go into an endless loop.
// The u32 is used for allocation sampling. It starts at 0 so that even shorter lived threads
// will at least get 1 allocation sampled.
thread_local! {
    static PROFILER_TL: YingThreadLocal = YingThreadLocal::new();
}

/// A struct to provide a better API around the lock out profiler flag/re-entrancy plus sampling
/// This is meant to be used ONLY in a thread-local and is definitely not multi-thread safe.
struct YingThreadLocal {
    // Counts up for every time we enter a no-allocator critical section (ie where we have to touch
    // allocator state or cause an allocation within profiling-related code and don't want sampling
    // of re-entrant allocations done).  Nonzero prevents allocator from sampling.
    alloc_lock: Cell<u32>,
    sample_count: Cell<u32>,
}

impl YingThreadLocal {
    fn new() -> Self {
        Self {
            alloc_lock: Cell::new(0),
            sample_count: Cell::new(0),
        }
    }

    fn is_allocator_locked(&self) -> bool {
        self.alloc_lock.get() > 0
    }

    fn set_allocator_lock(&self) {
        self.alloc_lock.set(self.alloc_lock.get().saturating_add(1));
    }

    fn release_allocator_lock(&self) {
        self.alloc_lock.set(self.alloc_lock.get().saturating_sub(1));
    }

    /// Obtains the counter, checks for sampling ratio, and updates counter in one go
    fn should_sample(&self, ratio: u32) -> bool {
        let counter = self.sample_count.get();
        self.sample_count.set(counter + 1); // update counter for next sampling
        counter % ratio == 0
    }

    // Resets counter to 0 to guarantee next call to alloc() will sample.  TESTING ONLY
    fn test_only_reset_sampling_counter(&self) {
        self.sample_count.set(0);
    }
}

pub fn testing_only_guarantee_next_sample() {
    PROFILER_TL.with(|tl| tl.test_only_reset_sampling_counter());
}

/// Locks the profiler flag so that allocations are not profiled.
/// This is for non-profiler code such as debug prints that has to access the Dashmap or state
/// and could potentially cause deadlock problems with Dashmap for example.
fn lock_out_profiler<R>(func: impl FnOnce() -> R) -> R {
    PROFILER_TL.with(|tl_state| {
        tl_state.set_allocator_lock();
        let return_val = func();
        tl_state.release_allocator_lock();
        return_val
    })
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
            PROFILER_TL.with(|tl_state| {
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
                    YING_STATE
                        .stack_stats
                        .entry(stack_hash)
                        .and_modify(|stats| {
                            // 4. Update stats
                            stats.num_allocations += 1;
                            stats.allocated_bytes += layout.size() as u64;
                        })
                        .or_insert_with(|| {
                            // 3. Resolve symbols if needed (new stack entry)
                            stack.populate_symbol_map(&mut bt, &YING_STATE.symbol_map);
                            StackStats::new(stack, Some(layout.size() as u64))
                        });

                    // 4. Record allocation so we can track outstanding vs transient allocs
                    YING_STATE
                        .outstanding_allocs
                        .entry(alloc_ptr as u64)
                        .or_insert_with(|| (stack_hash, Clock::recent_since_epoch().as_millis()));

                    // -- End of core profiling section, no more allocations --
                    tl_state.release_allocator_lock();
                }
            })
        }
        alloc_ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
        TOTAL_RETAINED.fetch_sub(layout.size(), SeqCst);

        // Return immediately and skip rest of this if YING_STATE is not initialized.  It could cause
        // an infinite loop because during initialization of YING_STATE, dealloc() could be then called
        if Lazy::get(&YING_STATE).is_none() {
            return;
        }

        // If the allocation was recorded in outstanding_allocs, then remove it and update stats
        // about number of bytes freed etc.  Do this with protection to guard against possible re-entry.
        if YING_STATE.outstanding_allocs.contains_key(&(ptr as u64)) {
            PROFILED_RETAINED.fetch_sub(layout.size(), SeqCst);
            PROFILER_TL.with(|tl_state| {
                if !tl_state.is_allocator_locked() {
                    tl_state.set_allocator_lock();

                    // -- Beginning of section that may allocate
                    if let Some((_, (stack_hash, _alloc_ts))) =
                        YING_STATE.outstanding_allocs.remove(&(ptr as u64))
                    {
                        // Update memory profiling freed bytes stats
                        YING_STATE
                            .stack_stats
                            .entry(stack_hash)
                            .and_modify(|stats| {
                                stats.freed_bytes += layout.size() as u64;
                                stats.num_frees += 1;
                            });

                        // TODO: see how long allocation was for, and update stats about how long lived
                    }

                    // -- End of core profiling section, no more allocations --
                    tl_state.release_allocator_lock();
                }
            });
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
            if Lazy::get(&YING_STATE).is_some()
                && YING_STATE.outstanding_allocs.contains_key(&(ptr as u64))
            {
                if new_size > old_size {
                    PROFILED_RETAINED.fetch_add(new_size - old_size, SeqCst);
                } else {
                    PROFILED_RETAINED.fetch_sub(old_size - new_size, SeqCst);
                }

                PROFILER_TL.with(|tl_state| {
                    if !tl_state.is_allocator_locked() {
                        tl_state.set_allocator_lock();

                        // -- Beginning of section that may allocate
                        if let Some((_, (stack_hash, alloc_ts))) =
                            YING_STATE.outstanding_allocs.remove(&(ptr as u64))
                        {
                            YING_STATE
                                .outstanding_allocs
                                .insert(new_ptr as u64, (stack_hash, alloc_ts));

                            // Update memory profiling freed bytes stats
                            YING_STATE
                                .stack_stats
                                .entry(stack_hash)
                                .and_modify(|stats| {
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
                });
            }
        }
        new_ptr
    }
}
