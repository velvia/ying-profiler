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
//! many different places in the code.  This is from `examples/tai_example.rs`:
//!
//! ```
//! Some(tai_example::insert_one::{{closure}}::h7eddb5f8ebb3289b)
//!  > Some(<core::future::from_generator::GenFuture<T> as core::future::future::Future>::poll::h7a53098577c44da0)
//!  > Some(tai_example::cache_update_loop::{{closure}}::h38556c7e7ae06bfa)
//!  > Some(<core::future::from_generator::GenFuture<T> as core::future::future::Future>::poll::hd319a0f603a1d426)
//!  > Some(tai_example::main::{{closure}}::h33aa63760e836e2f)
//!  > Some(<core::future::from_generator::GenFuture<T> as core::future::future::Future>::poll::hb2fd3cb904946c24)
//! ```
//!
//! A generic tool which just examines the IP and tries to figure out a single symbol would miss out on all of the
//! inlined symbols.  Some tools can expand on symbols, but the results still aren't very good.
//!
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

use backtrace::{Backtrace, BacktraceFrame};
use dashmap::DashMap;
use once_cell::sync::Lazy;
use rand::rngs::SmallRng;
use rand::{RngCore, SeedableRng};

pub mod callstack;
use callstack::StdCallstack;

/// Allocation sampling ratio.  Eg: 500 means 1 in 500 allocations are sampled.
const DEFAULT_SAMPLING_RATIO: u32 = 500;
/// The number of frames at the top of the stack to skip.  Most of these have to do with
/// backtrace and this profiler infrastructure.  This number needs to be adjusted
/// depending on the implementation.
/// For release/bench builds with debug = 1 / strip = none, this should be 4.
/// For debug builds, this is about 9.
const TOP_FRAMES_TO_SKIP: usize = 4;

// A map for caching symbols in backtraces so we can mostly store u64's
type SymbolMap = DashMap<u64, BacktraceFrame>;

/// Tai is a memory profiling Allocator wrapper.
/// Tai is the Swahili word for an eagle.
pub struct TaiAllocator;

static ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static PROFILED: AtomicUsize = AtomicUsize::new(0);

impl TaiAllocator {
    pub fn total_allocated() -> usize {
        ALLOCATED.load(SeqCst)
    }

    // Total bytes allocated for profiled allocations
    pub fn profiled_bytes() -> usize {
        PROFILED.load(SeqCst)
    }

    pub fn symbol_map_size() -> usize {
        TAI_STATE.symbol_map.len()
    }

    /// Dumps out a report on the top k stack traces by bytes allocated
    pub fn print_top_k_stacks_by_bytes(k: usize) {
        let total_profiled_bytes = PROFILED.load(SeqCst);
        let stacks_by_alloc = stack_list_allocated_bytes_desc();
        stacks_by_alloc
            .iter()
            .take(k)
            .for_each(|&(stack_hash, bytes_allocated)| {
                let stats = TAI_STATE
                    .stack_stats
                    .get(&stack_hash)
                    .expect("Did stats get removed?");
                let pct = (bytes_allocated as f64) * 100.0 / (total_profiled_bytes as f64);
                println!(
                    "\n---\n{} bytes allocated ({pct:.2}%) ({} allocations)",
                    stats.allocated_bytes, stats.num_allocations
                );
                println!("{}", stats.stack.with_symbols(&TAI_STATE.symbol_map));
            })
    }
}

/// Central struct collecting stats about each stack trace
#[derive(Debug, Clone)]
struct StackStats {
    stack: StdCallstack,
    allocated_bytes: u64,
    num_allocations: u64,
}

impl StackStats {
    fn new(stack: StdCallstack, initial_alloc_bytes: Option<u64>) -> Self {
        Self {
            stack,
            allocated_bytes: initial_alloc_bytes.unwrap_or(0),
            num_allocations: initial_alloc_bytes.map(|_| 1).unwrap_or(0),
        }
    }
}

// Private state.  We can't put this in the main TaiAllocator struct as that one has to be const static
struct TaiState {
    symbol_map: SymbolMap,
    // Main map of stack hash to StackStats
    stack_stats: DashMap<u64, StackStats>,
}

// lazily initialized global state
static TAI_STATE: Lazy<TaiState> = Lazy::new(|| {
    let symbol_map = SymbolMap::with_capacity(1000);
    let stack_stats = DashMap::with_capacity(1000);
    TaiState {
        symbol_map,
        stack_stats,
    }
});

/// Returns a list of stack IDs (stack_hash, bytes_allocated) in order from highest
/// number of bytes allocated to lowest
fn stack_list_allocated_bytes_desc() -> Vec<(u64, u64)> {
    let mut items = Vec::new();
    for entry in &TAI_STATE.stack_stats {
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

unsafe impl GlobalAlloc for TaiAllocator {
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
                        TAI_STATE
                            .stack_stats
                            .entry(stack_hash)
                            .and_modify(|stats| {
                                // 4. Update stats
                                stats.num_allocations += 1;
                                stats.allocated_bytes += layout.size() as u64;
                            })
                            .or_insert_with(|| {
                                // 3. Resolve symbols if needed (new stack entry)
                                stack.populate_symbol_map(&mut bt, &TAI_STATE.symbol_map);
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
