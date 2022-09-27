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
//! ```
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
//!
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::time::Duration;

use backtrace::Backtrace;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use rand::rngs::SmallRng;
use rand::{RngCore, SeedableRng};

pub mod callstack;
use callstack::{FriendlySymbol, StdCallstack};

/// Allocation sampling ratio.  Eg: 500 means 1 in 500 allocations are sampled.
const DEFAULT_SAMPLING_RATIO: u32 = 500;
/// The number of frames at the top of the stack to skip.  Most of these have to do with
/// backtrace and this profiler infrastructure.  This number needs to be adjusted
/// depending on the implementation.
/// For release/bench builds with debug = 1 / strip = none, this should be 4.
/// For debug builds, this is about 9.
const TOP_FRAMES_TO_SKIP: usize = 4;

// A map for caching symbols in backtraces so we can mostly store u64's
type SymbolMap = DashMap<u64, Vec<FriendlySymbol>>;

/// Ying is a memory profiling Allocator wrapper.
/// Ying is the Chinese word for an eagle.
pub struct YingProfiler;

static ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static PROFILED: AtomicUsize = AtomicUsize::new(0);

impl YingProfiler {
    pub fn total_allocated() -> usize {
        ALLOCATED.load(SeqCst)
    }

    // Total bytes allocated for profiled allocations
    pub fn profiled_bytes() -> usize {
        PROFILED.load(SeqCst)
    }

    pub fn symbol_map_size() -> usize {
        YING_STATE.symbol_map.len()
    }

    /// Dumps out a report on the top k stack traces by bytes allocated
    /// Defaults to symbols only, but with_filenames causes filenames/line#'s to be printed
    pub fn print_top_k_stacks_by_bytes(k: usize, with_filenames: bool) {
        let total_profiled_bytes = PROFILED.load(SeqCst);
        lock_out_profiler(|| {
            let stacks_by_alloc = stack_list_allocated_bytes_desc();
            stacks_by_alloc
                .iter()
                .take(k)
                .for_each(|&(stack_hash, bytes_allocated)| {
                    // Clone the value so we don't hang on to the reference.  This
                    // is important to avoid hanging on to reference across allocations
                    // which might create deadlocks
                    let stats = get_stats_for_stack_hash(stack_hash);
                    let pct = (bytes_allocated as f64) * 100.0 / (total_profiled_bytes as f64);
                    println!(
                        "\n---\n{} bytes allocated ({pct:.2}%) ({} allocations)",
                        stats.allocated_bytes, stats.num_allocations
                    );
                    #[cfg(feature = "profile-spans")]
                    if !stats.span.is_disabled() {
                        println!("\ttracing span id: {:?}", stats.span.id());
                    }
                    let decorated_stack = if with_filenames {
                        stats
                            .stack
                            .with_symbols_and_filename(&YING_STATE.symbol_map)
                    } else {
                        stats.stack.with_symbols(&YING_STATE.symbol_map)
                    };
                    println!("{}", decorated_stack);
                })
        })
    }
}

/// Central struct collecting stats about each stack trace
#[derive(Debug, Clone)]
struct StackStats {
    stack: StdCallstack,
    allocated_bytes: u64,
    num_allocations: u64,
    #[cfg(feature = "profile-spans")]
    span: tracing::Span,
}

impl StackStats {
    fn new(stack: StdCallstack, initial_alloc_bytes: Option<u64>) -> Self {
        Self {
            stack,
            allocated_bytes: initial_alloc_bytes.unwrap_or(0),
            num_allocations: initial_alloc_bytes.map(|_| 1).unwrap_or(0),
            #[cfg(feature = "profile-spans")]
            span: tracing::Span::current(),
        }
    }
}

// Private state.  We can't put this in the main YingProfiler struct as that one has to be const static
struct YingState {
    symbol_map: SymbolMap,
    // Main map of stack hash to StackStats
    stack_stats: DashMap<u64, StackStats>,
}

// lazily initialized global state
static YING_STATE: Lazy<YingState> = Lazy::new(|| {
    let symbol_map = SymbolMap::with_capacity(1000);
    let stack_stats = DashMap::with_capacity(1000);
    YingState {
        symbol_map,
        stack_stats,
    }
});

fn get_stats_for_stack_hash(stack_hash: u64) -> StackStats {
    YING_STATE
        .stack_stats
        .get(&stack_hash)
        .expect("Did stats get removed?")
        .value()
        .clone()
}

/// Returns a list of stack IDs (stack_hash, bytes_allocated) in order from highest
/// number of bytes allocated to lowest
fn stack_list_allocated_bytes_desc() -> Vec<(u64, u64)> {
    let mut items = Vec::new();
    for entry in &YING_STATE.stack_stats {
        items.push((*entry.key(), entry.value().allocated_bytes));
    }
    items.sort_unstable_by(|a, b| b.1.cmp(&a.1));
    items
}

// NOTE: The creation of state in this TL must NOT allocate. Otherwise it will cause
// the profiler code to go into an endless loop.
thread_local! {
    static PROFILER_TL: RefCell<(bool, SmallRng)> = RefCell::new((false, SmallRng::from_entropy()));
}

/// Locks the profiler flag so that allocations are not profiled.
/// This is for non-profiler code such as debug prints that has to access the Dashmap or state
/// and could potentially cause deadlock problems with Dashmap for example.
fn lock_out_profiler<R>(func: impl FnOnce() -> R) -> R {
    PROFILER_TL.with(|tl_state| {
        // Within the same thread, nobody else should be holding the profiler lock here,
        // but we'll check just to be sure
        while tl_state.borrow().0 {
            std::thread::sleep(Duration::from_millis(2));
        }

        tl_state.borrow_mut().0 = true;
        let return_val = func();
        tl_state.borrow_mut().0 = false;
        return_val
    })
}

unsafe impl GlobalAlloc for YingProfiler {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // NOTE: the code between here and the state.0 = true must be re-entrant
        // and therefore not allocate, otherwise there will be an infinite loop.
        let ret = System.alloc(layout);
        if !ret.is_null() {
            ALLOCATED.fetch_add(layout.size(), SeqCst);

            // Now, sample allocation - if it falls below threshold, then profile
            // Also, we set a ThreadLocal to avoid re-entry: ie the code below might allocate,
            // and we avoid profiling if we are already in the loop below.  Avoids cycles.
            PROFILER_TL.with(|tl_state| {
                // We do the following in two steps because we cannot borrow_mut() twice
                // if profiler code allocates
                if !tl_state.borrow().0 {
                    let mut state = tl_state.borrow_mut();
                    if (state.1.next_u32() % DEFAULT_SAMPLING_RATIO) == 0 {
                        state.0 = true;
                        // This drop is important for re-entry purposes
                        drop(state);

                        PROFILED.fetch_add(layout.size(), SeqCst);

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

                        // -- End of core profiling section, no more allocations --
                        tl_state.borrow_mut().0 = false;
                    }
                }
            })
        }
        ret
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
        ALLOCATED.fetch_sub(layout.size(), SeqCst);
    }
}
